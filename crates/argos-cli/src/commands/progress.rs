//! How `argos` renders a run in a terminal.
//!
//! All that is left of what used to be `commands::helper`: locating and
//! elevating `argos-helper`, and parsing its event stream, now live in
//! `argos-session`, so a GUI drives the identical run without reimplementing
//! them. What stayed here is the part that is genuinely about a terminal.

use argos_session::human_size;
use argos_session::{EventSink, Outcome, SessionEvent};
use indicatif::{ProgressBar, ProgressStyle};
use std::time::{Duration, Instant};

/// Renders progress as an `indicatif` bar when stdout is a real, attended
/// terminal, and falls back to periodic plain-text status lines otherwise --
/// `indicatif` itself just draws nothing when stdout is piped/redirected
/// (backlog #16), which for a multi-gigabyte write leaves long stretches
/// with no way to tell "still working" from "hung" in any non-interactive
/// context (a log file, `tee`, a CI job). The underlying event stream
/// `argos-helper` emits never changes either way -- only this rendering
/// does.
pub enum Presenter {
    Bar(ProgressBar),
    Plain(PlainPresenter),
}

impl Presenter {
    pub fn new() -> Self {
        if console::Term::stdout().features().is_attended() {
            let bar = ProgressBar::new(0);
            bar.set_style(
                ProgressStyle::with_template("{msg} [{bar:40}] {bytes}/{total_bytes} ({eta})")
                    .unwrap_or_else(|_| ProgressStyle::default_bar())
                    .progress_chars("=> "),
            );
            Presenter::Bar(bar)
        } else {
            Presenter::Plain(PlainPresenter::new())
        }
    }

    fn set_phase(&mut self, phase: String) {
        match self {
            Presenter::Bar(bar) => bar.set_message(phase),
            Presenter::Plain(plain) => plain.on_phase(&phase),
        }
    }

    fn set_progress(&mut self, bytes_done: u64, bytes_total: u64) {
        match self {
            Presenter::Bar(bar) => {
                bar.set_length(bytes_total);
                bar.set_position(bytes_done);
            }
            Presenter::Plain(plain) => plain.on_progress(bytes_done, bytes_total),
        }
    }

    fn finish(&mut self, message: &'static str) {
        match self {
            Presenter::Bar(bar) => bar.finish_with_message(message),
            Presenter::Plain(plain) => plain.on_finish(message),
        }
    }

    fn abandon(&mut self) {
        if let Presenter::Bar(bar) = self {
            bar.abandon();
        }
        // Nothing to clean up for plain lines -- there's no in-place bar
        // state to leave in a stale-looking position.
    }
}

impl EventSink for Presenter {
    fn on_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Phase(phase) => self.set_phase(phase),
            SessionEvent::Progress {
                bytes_done,
                bytes_total,
            } => self.set_progress(bytes_done, bytes_total),
            // Arrives after the terminal event, so the bar is already
            // finished and printing here can't garble it. The write's own
            // success is unaffected either way.
            SessionEvent::Ejected { device_path, error } => match error {
                None => println!("Ejected {device_path}. Safe to unplug."),
                Some(err) => eprintln!(
                    "warning: could not eject {device_path}: {err} (the write itself succeeded -- eject it manually before unplugging)"
                ),
            },
        }
    }

    fn on_finished(&mut self, outcome: &Outcome) {
        // The two words the bar has always ended with; which one depends
        // only on whether anything was written.
        self.finish(if outcome.is_verify() {
            "verified"
        } else {
            "done"
        });
    }

    fn on_failed(&mut self) {
        self.abandon();
    }
}

/// Prints one line per phase change, plus periodic progress lines throttled
/// to roughly every 5 percentage points *or* every few seconds, whichever
/// comes first -- frequent enough that a long write never looks hung, rare
/// enough not to flood a log file.
pub struct PlainPresenter {
    current_phase: String,
    last_reported_percent: i64,
    last_reported_at: Instant,
    /// Set on every phase change, cleared on the next progress report --
    /// forces that first report to print unconditionally, rather than
    /// leaving it to `last_reported_percent`'s arithmetic (which only
    /// guarantees *a* report within the next 5 points, not necessarily on
    /// the very first one, if the new phase's first progress event happens
    /// to land inside that window).
    phase_just_changed: bool,
}

const PLAIN_PROGRESS_PERCENT_STEP: i64 = 5;
const PLAIN_PROGRESS_MIN_INTERVAL: Duration = Duration::from_secs(3);

impl PlainPresenter {
    fn new() -> Self {
        Self {
            current_phase: String::new(),
            last_reported_percent: -1,
            last_reported_at: Instant::now(),
            phase_just_changed: true,
        }
    }

    fn on_phase(&mut self, phase: &str) {
        self.current_phase = phase.to_string();
        self.phase_just_changed = true;
        println!("{phase}...");
    }

    fn on_progress(&mut self, bytes_done: u64, bytes_total: u64) {
        if bytes_total == 0 {
            return;
        }
        let percent = (bytes_done * 100 / bytes_total) as i64;
        if !self.should_report(percent) {
            return;
        }
        println!(
            "{}: {percent}% ({} / {})",
            self.current_phase,
            human_size(bytes_done),
            human_size(bytes_total)
        );
        self.last_reported_percent = percent;
        self.last_reported_at = Instant::now();
        self.phase_just_changed = false;
    }

    /// Pure throttling decision, kept separate from `on_progress`'s `println!`
    /// so it's unit-testable without capturing stdout or waiting on a real
    /// clock: always report right after a phase change; otherwise report at
    /// `percent` when it advanced far enough past the last reported value,
    /// or enough wall-clock time has passed, or the operation just finished
    /// (100% always reports, even if it lands mid-bucket).
    fn should_report(&self, percent: i64) -> bool {
        if self.phase_just_changed {
            return true;
        }
        let percent_advanced = percent >= self.last_reported_percent + PLAIN_PROGRESS_PERCENT_STEP;
        let enough_time_passed = self.last_reported_at.elapsed() >= PLAIN_PROGRESS_MIN_INTERVAL;
        let finished = percent >= 100;
        percent_advanced || enough_time_passed || finished
    }

    fn on_finish(&mut self, message: &str) {
        println!("{message}.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A presenter as it looks right after some progress has already been
    /// reported -- the common case `should_report`'s throttling logic
    /// actually has to make a decision in, as opposed to the always-report
    /// state right after construction or a phase change.
    fn settled_presenter(last_reported_percent: i64) -> PlainPresenter {
        PlainPresenter {
            current_phase: "Writing".into(),
            last_reported_percent,
            last_reported_at: Instant::now(),
            phase_just_changed: false,
        }
    }

    #[test]
    fn fresh_presenter_reports_immediately_on_first_progress() {
        // Never actually reached in practice (argos-helper always emits a
        // Phase before any Progress for that phase), but a safe default if
        // it ever were: report rather than silently swallow the first byte
        // count.
        let presenter = PlainPresenter::new();
        assert!(presenter.should_report(0));
    }

    #[test]
    fn reports_once_percent_advances_by_the_configured_step() {
        let presenter = settled_presenter(10);
        assert!(!presenter.should_report(10 + PLAIN_PROGRESS_PERCENT_STEP - 1));
        assert!(presenter.should_report(10 + PLAIN_PROGRESS_PERCENT_STEP));
    }

    #[test]
    fn does_not_report_again_immediately_after_reporting() {
        let presenter = settled_presenter(10);
        assert!(!presenter.should_report(12));
    }

    #[test]
    fn always_reports_at_full_completion_even_mid_bucket() {
        let presenter = settled_presenter(97);
        assert!(presenter.should_report(100));
    }

    #[test]
    fn phase_change_forces_the_next_report_regardless_of_percent() {
        let mut presenter = settled_presenter(40);
        assert!(!presenter.should_report(41)); // mid-bucket, not yet due
        presenter.on_phase("Verifying");
        assert!(presenter.should_report(0)); // now due, unconditionally
    }

    #[test]
    fn reporting_progress_clears_the_phase_just_changed_flag() {
        let mut presenter = PlainPresenter::new();
        presenter.on_progress(1, 100);
        assert!(!presenter.phase_just_changed);
        assert_eq!(presenter.last_reported_percent, 1);
    }
}
