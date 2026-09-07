//! What a front end sees while `argos-helper` works, and what it gets back
//! when the helper is done.
//!
//! These types exist so no front end has to know the helper's wire format.
//! `argos-cli` used to consume `protocol::Event` directly *and* build its
//! result strings inside the stream loop; splitting the two is what lets a
//! GUI render the same run without reimplementing either half.

use argos_privileged::protocol::PhaseWire;

/// Progress, as a front end cares about it.
///
/// The phase arrives as a [`PhaseWire`], so a front end can *match* on
/// `Known(Phase)` to label it in the user's own language, and still has the
/// raw string for the `Unknown` case a version-skewed helper could produce.
/// How it is worded is presentation, and stays with the front end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Phase(PhaseWire),
    Progress {
        bytes_done: u64,
        bytes_total: u64,
    },
    /// The post-write eject the plan asked for. Arrives *after* the terminal
    /// event, and never changes the outcome: by then the write has succeeded
    /// and been verified, so a device that will not eject is a warning about
    /// unplugging, not a bad write.
    Ejected {
        device_path: String,
        error: Option<String>,
    },
}

/// What the helper reported on success. Carries the numbers, not a sentence:
/// how they are worded is presentation, and belongs to whichever front end is
/// doing the talking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    DdWrite {
        hash: String,
    },
    Verify {
        hash: String,
    },
    WindowsWrite {
        files_copied: u64,
        bytes_copied: u64,
    },
    WindowsVerify {
        files_verified: u64,
    },
}

impl Outcome {
    /// Whether this outcome came from a read-only verify rather than a write
    /// -- the one thing a progress renderer needs from it, and the reason
    /// `argos-cli`'s bar finishes with "verified" instead of "done".
    pub fn is_verify(&self) -> bool {
        matches!(self, Outcome::Verify { .. } | Outcome::WindowsVerify { .. })
    }
}

/// Where a front end receives the run as it happens.
///
/// `on_finished`/`on_failed` are separate from `on_event` because a terminal
/// event is not progress: `argos-cli` finishes or abandons its progress bar
/// there, and a GUI switches panels.
pub trait EventSink {
    fn on_event(&mut self, event: SessionEvent);
    fn on_finished(&mut self, outcome: &Outcome) {
        let _ = outcome;
    }
    fn on_failed(&mut self) {}
}

/// A sink that discards everything, for callers that only want the outcome.
pub struct NoopSink;
impl EventSink for NoopSink {
    fn on_event(&mut self, _event: SessionEvent) {}
}

impl<F: FnMut(SessionEvent)> EventSink for F {
    fn on_event(&mut self, event: SessionEvent) {
        self(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_verify_outcomes_report_themselves_as_verifies() {
        assert!(Outcome::Verify { hash: "x".into() }.is_verify());
        assert!(Outcome::WindowsVerify { files_verified: 3 }.is_verify());
        assert!(!Outcome::DdWrite { hash: "x".into() }.is_verify());
        assert!(!Outcome::WindowsWrite {
            files_copied: 1,
            bytes_copied: 2
        }
        .is_verify());
    }

    #[test]
    fn a_closure_can_serve_as_a_sink() {
        use argos_core::progress::Phase;
        let mut seen = Vec::new();
        {
            let mut sink = |event: SessionEvent| seen.push(event);
            sink.on_event(SessionEvent::Phase(PhaseWire::Known(Phase::Writing)));
        }
        assert_eq!(
            seen,
            vec![SessionEvent::Phase(PhaseWire::Known(Phase::Writing))]
        );
    }
}
