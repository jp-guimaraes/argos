//! What the window is doing, as a value.
//!
//! Kept as a plain enum plus a free [`reduce`] function, rather than methods
//! that reach into widgets, so every transition is testable without opening a
//! window -- which matters because CI is headless and because the two rules
//! that most need pinning here (when Write may be pressed, and when Cancel
//! can still do anything) are both safety-adjacent.

use argos_core::device::Device;
use argos_core::error::ArgosError;
use argos_core::progress::Phase;
use argos_session::{Canceller, Outcome, PhaseWire, PreparedWrite, SessionEvent};

#[derive(Default)]
pub enum AppState {
    /// Choosing an image and a device.
    #[default]
    Idle,
    /// `prepare_write` is running on a worker: classify, preflight, plan.
    Preparing,
    /// Everything checked out; the user is being asked to retype the device
    /// path. Nothing destructive has happened yet.
    AwaitingConfirmation {
        prepared: Box<PreparedWrite>,
        typed: String,
    },
    /// Reached once G5 (#92) wires the confirmation to `session::spawn`.
    ///
    /// Present already, and covered by the tests below, because the rule it
    /// carries is the one #104 measured: a Cancel button must go inert the
    /// moment the run leaves the copy loop. That rule belongs to the state
    /// machine, and building the state machine first means G5 wires a
    /// transition rather than retrofitting a guarantee. Not wired here
    /// because G5's own acceptance -- a real write from the window -- is
    /// blocked on #102 until the macOS elevation question is settled.
    #[allow(
        dead_code,
        reason = "constructed by G5 (#92); the rule it carries is tested now"
    )]
    Running(RunState),
    Done {
        outcome: Outcome,
        device_id: String,
    },
    Failed {
        error: ArgosError,
    },
    Cancelled,
}

#[derive(Default)]
pub struct RunState {
    /// Carried so the Done panel can name the drive after the run, when the
    /// selection may already have been cleared.
    pub device_id: String,
    /// `None` until the elevated helper is actually up. A Cancel press before
    /// that has nothing to write to, which is correct: nothing is running.
    pub canceller: Option<Canceller>,
    pub phase: Option<Phase>,
    /// The phase label as it arrived, kept for the version-skew case where
    /// the helper sent a name this build does not know.
    pub phase_label: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub cancel_requested: bool,
    /// What the helper reported about ejecting, once it has.
    pub eject_note: Option<String>,
}

/// What a worker thread sends back to the UI thread.
pub enum WorkerMsg {
    Devices(Result<Vec<Device>, ArgosError>),
    /// What kind of image was picked, judged without a device. Handled by the
    /// app rather than the reducer: it describes the *inputs*, not what the
    /// app is doing.
    Classified(Result<Option<argos_session::ImageKind>, ArgosError>),
    Prepared(Result<Box<PreparedWrite>, ArgosError>),
    /// The elevated helper is up; this is the handle a Cancel press drives.
    #[allow(dead_code, reason = "sent by G5 (#92); the reduction is tested now")]
    Started(Canceller),
    #[allow(dead_code, reason = "sent by G5 (#92); the reduction is tested now")]
    Event(SessionEvent),
    #[allow(dead_code, reason = "sent by G5 (#92); the reduction is tested now")]
    Finished(Result<Outcome, ArgosError>),
}

/// Whether a cancel request can still take effect.
///
/// Not cosmetic, and not a guess: measured on real hardware (#104, and the
/// comment above `flush_write` in `argos-privileged`). The `CancelToken`
/// reaches the DD copy loop and the FAT32 file copy, and **nothing else** --
/// not the unmount, not the fsync, not the read-back verify, not
/// partitioning or formatting. Writing a 5.9GiB ISO to a USB 3.2 stick, the
/// copy loop was 7.8s of 320s: a button that stays live through the other
/// 98% is telling the user something untrue, and a cancel pressed there is
/// discarded in silence rather than delayed.
///
/// `None` means the helper has not announced a phase yet, which is still
/// inside the cancellable part -- nothing has been written.
pub fn is_cancellable(phase: Option<Phase>) -> bool {
    match phase {
        None => true,
        Some(Phase::Writing) | Some(Phase::CopyingFiles) => true,
        Some(
            Phase::Unmounting
            | Phase::Checksumming
            | Phase::Flushing
            | Phase::Verifying
            | Phase::Partitioning
            | Phase::FormattingFat32,
        ) => false,
    }
}

/// Whether the destructive action may proceed.
///
/// The GUI half of the retype-the-device-path guard. Case-sensitive on
/// purpose: `/dev/SDB` is not `/dev/sdb`, and the whole point of the guard is
/// that the user looked at the path rather than pattern-matched it.
/// Surrounding whitespace is forgiven because a trailing space is a typing
/// artefact, not a misreading.
pub fn confirmation_matches(typed: &str, platform_id: &str) -> bool {
    !platform_id.is_empty() && typed.trim() == platform_id
}

pub fn reduce(state: AppState, msg: WorkerMsg) -> AppState {
    match (state, msg) {
        // These describe the inputs, not what the app is doing, and the app
        // keeps them itself. Passing them through here would mean every state
        // having to hand them back unchanged.
        (state, WorkerMsg::Devices(_) | WorkerMsg::Classified(_)) => state,

        (AppState::Preparing, WorkerMsg::Prepared(Ok(prepared))) => {
            AppState::AwaitingConfirmation {
                prepared,
                typed: String::new(),
            }
        }
        (AppState::Preparing, WorkerMsg::Prepared(Err(error))) => AppState::Failed { error },
        // A preparation that lands after the user has moved on (pressed Back,
        // or started another) is stale; dropping it is right, and panicking
        // on it would be a crash triggered by ordinary timing.
        (state, WorkerMsg::Prepared(_)) => state,

        (AppState::Running(mut run), WorkerMsg::Started(canceller)) => {
            // A cancel pressed before the helper was up: honour it now that
            // there is something to honour.
            if run.cancel_requested {
                canceller.cancel();
            }
            run.canceller = Some(canceller);
            AppState::Running(run)
        }

        (AppState::Running(mut run), WorkerMsg::Event(event)) => {
            apply_event(&mut run, event);
            AppState::Running(run)
        }

        (AppState::Running(run), WorkerMsg::Finished(Ok(outcome))) => AppState::Done {
            outcome,
            device_id: run.device_id_hint(),
        },
        (AppState::Running(run), WorkerMsg::Finished(Err(error))) => {
            // A helper that stopped because we asked reports `Cancelled`; the
            // distinction matters because "you cancelled" is not a failure to
            // apologise for. Both the request and the helper's own verdict
            // have to agree, so a genuine mid-write I/O error during a
            // cancel-that-did-not-take is still shown as the error it was.
            if run.cancel_requested && matches!(error, ArgosError::Cancelled) {
                AppState::Cancelled
            } else {
                AppState::Failed { error }
            }
        }

        // Anything else arriving in a state that cannot use it -- progress
        // after the run already settled, a second `Started` -- is ignored
        // rather than treated as a bug. Threads finish in their own time.
        (state, _) => state,
    }
}

fn apply_event(run: &mut RunState, event: SessionEvent) {
    match event {
        SessionEvent::Phase(wire) => {
            run.phase = match &wire {
                PhaseWire::Known(phase) => Some(*phase),
                PhaseWire::Unknown(_) => None,
            };
            run.phase_label = match wire {
                PhaseWire::Known(phase) => format!("{phase:?}"),
                PhaseWire::Unknown(raw) => raw,
            };
        }
        SessionEvent::Progress {
            bytes_done,
            bytes_total,
        } => {
            run.bytes_done = bytes_done;
            run.bytes_total = bytes_total;
        }
        SessionEvent::Ejected { device_path, error } => {
            run.eject_note = Some(match error {
                None => format!("Ejected {device_path}. Safe to unplug."),
                Some(err) => format!("Could not eject {device_path}: {err}"),
            });
        }
    }
}

impl RunState {
    #[allow(
        dead_code,
        reason = "constructed by G5 (#92); exercised by the tests below"
    )]
    pub fn new(device_id: String, bytes_total: u64) -> Self {
        RunState {
            bytes_total,
            device_id: device_id.clone(),
            ..Default::default()
        }
    }

    fn device_id_hint(&self) -> String {
        self.device_id.clone()
    }

    /// Fraction written, for the progress bar. `None` while the helper has
    /// not said how much there is to do -- an indeterminate bar is honest
    /// where a 0% one implies it has started and stalled.
    pub fn fraction(&self) -> Option<f32> {
        (self.bytes_total > 0).then(|| self.bytes_done as f32 / self.bytes_total as f32)
    }

    pub fn is_cancellable(&self) -> bool {
        !self.cancel_requested && is_cancellable(self.phase)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argos_core::device::Bus;

    fn running() -> AppState {
        AppState::Running(RunState::new("/dev/sdz".into(), 100))
    }

    fn usb_stick() -> Device {
        Device {
            platform_id: "/dev/sdz".into(),
            display_name: "Example USB Stick".into(),
            size_bytes: 8_000_000_000,
            bus: Bus::Usb,
            os_reports_removable: true,
            is_system_disk: false,
            serial: Some("ABC123".into()),
        }
    }

    // ---- the retype guard ------------------------------------------------

    /// The GUI half of the guard the CLI has always had. Deserves the same
    /// care as `check_device_is_offerable`: this is what stands between a
    /// mis-click and the wrong disk.
    #[test]
    fn confirmation_requires_the_exact_device_path() {
        assert!(confirmation_matches("/dev/sdz", "/dev/sdz"));
        assert!(confirmation_matches("  /dev/sdz \n", "/dev/sdz"));
        assert!(!confirmation_matches("", "/dev/sdz"));
        assert!(!confirmation_matches("/dev/sd", "/dev/sdz"));
        assert!(!confirmation_matches("/dev/sdz1", "/dev/sdz"));
        assert!(!confirmation_matches("sdz", "/dev/sdz"));
    }

    /// Case-sensitive on purpose: the guard exists so the user *reads* the
    /// path, and `/dev/SDZ` is evidence they did not.
    #[test]
    fn confirmation_is_case_sensitive() {
        assert!(!confirmation_matches("/dev/SDZ", "/dev/sdz"));
    }

    /// Never let an empty target be confirmable by an empty box.
    #[test]
    fn an_empty_device_path_can_never_be_confirmed() {
        assert!(!confirmation_matches("", ""));
        assert!(!confirmation_matches("   ", ""));
    }

    // ---- the cancellable window (#104) -----------------------------------

    /// Pins the measurement, not a guess: the token reaches the DD copy loop
    /// and the FAT32 file copy, and nothing else. Exhaustive over `Phase`, so
    /// adding a variant forces a decision here rather than defaulting to
    /// "looks cancellable".
    #[test]
    fn only_the_two_phases_that_consult_the_token_are_cancellable() {
        assert!(is_cancellable(Some(Phase::Writing)));
        assert!(is_cancellable(Some(Phase::CopyingFiles)));
        for phase in [
            Phase::Unmounting,
            Phase::Checksumming,
            Phase::Flushing,
            Phase::Verifying,
            Phase::Partitioning,
            Phase::FormattingFat32,
        ] {
            assert!(
                !is_cancellable(Some(phase)),
                "{phase:?} does not consult it"
            );
        }
    }

    /// Before the helper announces anything, nothing has been written, so
    /// stopping is both possible and free.
    #[test]
    fn a_run_with_no_phase_yet_is_cancellable() {
        assert!(is_cancellable(None));
    }

    /// The button must go inert the moment the run leaves the copy loop --
    /// this is the whole point of #104.
    #[test]
    fn the_button_dies_when_the_run_reaches_flushing() {
        let mut run = RunState::new("/dev/sdz".into(), 100);
        run.phase = Some(Phase::Writing);
        assert!(run.is_cancellable());
        run.phase = Some(Phase::Flushing);
        assert!(!run.is_cancellable(), "a fsync cannot be interrupted");
    }

    #[test]
    fn a_run_already_asked_to_stop_does_not_offer_to_stop_again() {
        let mut run = RunState::new("/dev/sdz".into(), 100);
        run.phase = Some(Phase::Writing);
        run.cancel_requested = true;
        assert!(!run.is_cancellable());
    }

    // ---- the reducer -----------------------------------------------------

    /// The whole unprivileged chain, composed from the window's side: a real
    /// `prepare_write` against a fake platform and a synthetic Windows ISO,
    /// reduced into the confirmation state.
    ///
    /// Driven through the real function rather than a hand-made value on
    /// purpose -- `PreparedWrite` keeps its `Plan` private so a front end
    /// cannot fabricate one and skip the checks.
    #[test]
    fn a_successful_preparation_asks_for_confirmation_with_an_empty_box() {
        let iso = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            iso.path(),
            argos_core::image::windows::fixtures::udf_windows_installer_iso(true, true),
        )
        .unwrap();

        let platform = argos_platform::fake::FakePlatform::new(vec![usb_stick()]);
        let prepared = argos_session::prepare_write(
            &platform,
            &argos_session::WriteRequest {
                iso: iso.path().to_path_buf(),
                device_id: "/dev/sdz".into(),
                no_verify: false,
                eject: true,
                allow_non_removable: false,
                layout: argos_privileged::protocol::WindowsLayout::Fat32,
            },
        )
        .expect("an ordinary USB stick and a Windows installer must prepare cleanly");

        let state = reduce(
            AppState::Preparing,
            WorkerMsg::Prepared(Ok(Box::new(prepared))),
        );
        let AppState::AwaitingConfirmation { prepared, typed } = state else {
            panic!("expected the confirmation step")
        };
        // The box starts empty: the guard is only satisfied by typing.
        assert!(typed.is_empty());
        assert!(!confirmation_matches(&typed, &prepared.device.platform_id));
        assert_eq!(prepared.device.platform_id, "/dev/sdz");
    }

    /// A refused device must not reach the confirmation step at all -- the
    /// window never gets a chance to ask about a disk `prepare_write`
    /// rejected.
    #[test]
    fn a_refused_preparation_never_reaches_the_confirmation_step() {
        let state = reduce(
            AppState::Preparing,
            WorkerMsg::Prepared(Err(ArgosError::DeviceIsSystemDisk("/dev/sda".into()))),
        );
        assert!(matches!(state, AppState::Failed { .. }));
    }

    /// Progress arriving in a state that cannot use it is ordinary thread
    /// timing, not a bug: it must be dropped, never panic.
    #[test]
    fn stray_progress_outside_a_run_is_ignored() {
        let state = reduce(
            AppState::Idle,
            WorkerMsg::Event(SessionEvent::Progress {
                bytes_done: 1,
                bytes_total: 2,
            }),
        );
        assert!(matches!(state, AppState::Idle));
    }

    #[test]
    fn a_device_list_never_changes_what_the_app_is_doing() {
        let state = reduce(
            AppState::Preparing,
            WorkerMsg::Devices(Ok(vec![usb_stick()])),
        );
        assert!(matches!(state, AppState::Preparing));
    }

    #[test]
    fn progress_and_phase_land_on_the_run() {
        let state = reduce(
            running(),
            WorkerMsg::Event(SessionEvent::Phase(PhaseWire::Known(Phase::Writing))),
        );
        let state = reduce(
            state,
            WorkerMsg::Event(SessionEvent::Progress {
                bytes_done: 25,
                bytes_total: 100,
            }),
        );
        let AppState::Running(run) = state else {
            panic!("still running")
        };
        assert_eq!(run.phase, Some(Phase::Writing));
        assert_eq!(run.phase_label, "Writing");
        assert_eq!(run.fraction(), Some(0.25));
    }

    /// A phase name this build does not know still shows *something*, and is
    /// treated as not-cancellable because we cannot know that it is.
    #[test]
    fn an_unknown_phase_keeps_its_label_and_is_not_assumed_cancellable() {
        let state = reduce(
            running(),
            WorkerMsg::Event(SessionEvent::Phase(PhaseWire::Unknown("Polishing".into()))),
        );
        let AppState::Running(run) = state else {
            panic!("still running")
        };
        assert_eq!(run.phase_label, "Polishing");
        assert_eq!(run.phase, None);
    }

    #[test]
    fn an_eject_report_is_kept_for_the_result_panel() {
        let state = reduce(
            running(),
            WorkerMsg::Event(SessionEvent::Ejected {
                device_path: "/dev/sdz".into(),
                error: None,
            }),
        );
        let AppState::Running(run) = state else {
            panic!("still running")
        };
        assert!(run.eject_note.unwrap().contains("Safe to unplug"));
    }

    /// A cancel pressed before the helper is up must not be lost: it is
    /// replayed onto the handle the moment one exists.
    #[test]
    fn a_cancel_pressed_before_the_helper_started_is_replayed() {
        let mut run = RunState::new("/dev/sdz".into(), 100);
        run.cancel_requested = true;
        let state = reduce(
            AppState::Running(run),
            WorkerMsg::Started(Canceller::noop()),
        );
        let AppState::Running(run) = state else {
            panic!("still running")
        };
        assert!(run.canceller.is_some());
    }

    /// "You cancelled" is not a failure to apologise for -- but it takes both
    /// the request and the helper agreeing.
    #[test]
    fn a_cancelled_run_reports_as_cancelled_not_failed() {
        let mut run = RunState::new("/dev/sdz".into(), 100);
        run.cancel_requested = true;
        let state = reduce(
            AppState::Running(run),
            WorkerMsg::Finished(Err(ArgosError::Cancelled)),
        );
        assert!(matches!(state, AppState::Cancelled));
    }

    /// The case #104 makes real: the user pressed Cancel during the flush,
    /// where it was discarded, and the write finished anyway. That is a
    /// success, and must be reported as one rather than as a cancellation.
    #[test]
    fn a_cancel_that_arrived_too_late_still_reports_the_write_that_succeeded() {
        let mut run = RunState::new("/dev/sdz".into(), 100);
        run.cancel_requested = true;
        run.phase = Some(Phase::Flushing);
        let state = reduce(
            AppState::Running(run),
            WorkerMsg::Finished(Ok(Outcome::DdWrite { hash: "abc".into() })),
        );
        assert!(matches!(state, AppState::Done { .. }));
    }

    /// A real I/O error during a run the user also tried to cancel is still
    /// an error: only the helper's own `Cancelled` verdict earns the quiet
    /// wording.
    #[test]
    fn a_real_error_during_an_attempted_cancel_is_still_an_error() {
        let mut run = RunState::new("/dev/sdz".into(), 100);
        run.cancel_requested = true;
        let state = reduce(
            AppState::Running(run),
            WorkerMsg::Finished(Err(ArgosError::Helper {
                message: "input/output error".into(),
                exit_code: 19,
            })),
        );
        assert!(matches!(state, AppState::Failed { .. }));
    }

    #[test]
    fn a_finished_run_names_the_device_it_wrote() {
        let state = reduce(
            running(),
            WorkerMsg::Finished(Ok(Outcome::DdWrite { hash: "abc".into() })),
        );
        let AppState::Done { device_id, .. } = state else {
            panic!("done")
        };
        assert_eq!(device_id, "/dev/sdz");
    }
}
