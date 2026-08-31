//! Shared machinery for talking to the privileged `argos-helper` process:
//! locating the binary, elevating (`pkexec`/`sudo`), feeding it a `Plan` as
//! JSON on stdin, and turning its `Event` stream into human-visible
//! progress. Used by both `write` (which sends `Plan::Write`) and `verify`
//! (which sends `Plan::Verify`) -- the only difference between the two, from
//! here, is which terminal event carries the hash they print.

use argos_core::error::{ArgosError, Result};
use argos_privileged::protocol::{Event, Plan};
use indicatif::{ProgressBar, ProgressStyle};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Elevates, feeds `plan` to `argos-helper`, renders its progress, and
/// returns the hash it reported on success (`Event::Done`'s `written_hash`
/// for a write, `Event::VerifyOk`'s `hash` for a verify -- callers already
/// know which one they sent, so they don't need the `Event` back, only the
/// string).
pub fn run_plan(plan: &Plan) -> Result<String> {
    let helper_path = locate_helper_binary();
    let elevation_command = if cfg!(target_os = "linux") && command_exists("pkexec") {
        "pkexec"
    } else {
        "sudo"
    };

    let mut child = Command::new(elevation_command)
        .arg(&helper_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    {
        let stdin = child.stdin.as_mut().expect("stdin was piped");
        let plan_json = serde_json::to_string(plan).map_err(std::io::Error::other)?;
        stdin.write_all(plan_json.as_bytes())?;
    }
    // Dropping stdin (by taking it out of scope) signals EOF to the helper.
    drop(child.stdin.take());

    let stdout = child.stdout.take().expect("stdout was piped");
    let outcome = stream_helper_events(BufReader::new(stdout));

    let status = child.wait()?;
    match outcome {
        Some(Ok(hash)) => Ok(hash),
        Some(Err(err)) => Err(err),
        None if status.success() => Err(ArgosError::Io(std::io::Error::other(
            "argos-helper exited successfully but reported no result",
        ))),
        None => Err(ArgosError::Io(std::io::Error::other(format!(
            "argos-helper exited with {status} and reported no result"
        )))),
    }
}

/// Reads one JSON [`Event`] per line from the helper, driving a [`Presenter`],
/// until a terminal event settles the outcome (or the stream simply ends).
fn stream_helper_events<R: std::io::Read>(
    reader: BufReader<R>,
) -> Option<std::result::Result<String, ArgosError>> {
    let mut presenter = Presenter::new();

    let mut outcome = None;
    for line in reader.lines().map_while(std::result::Result::ok) {
        let Ok(event) = serde_json::from_str::<Event>(&line) else {
            continue;
        };
        match event {
            Event::Phase { phase } => presenter.set_phase(phase),
            Event::Progress {
                bytes_done,
                bytes_total,
            } => presenter.set_progress(bytes_done, bytes_total),
            Event::Done { written_hash } => {
                presenter.finish("done");
                outcome = Some(Ok(written_hash));
            }
            Event::VerifyOk { hash } => {
                presenter.finish("verified");
                outcome = Some(Ok(hash));
            }
            Event::WindowsDone {
                files_copied,
                bytes_copied,
            } => {
                // `run_plan`'s single-hash `Result<String>` contract doesn't
                // really fit a two-partition write (there's no one
                // meaningful whole-device hash -- see
                // `argos_privileged::windows::WindowsWriteOutcome`'s doc
                // comment). Nothing sends `Plan::WriteWindowsImage` yet (W5,
                // still pending); this is a placeholder summary just to keep
                // this match exhaustive until the CLI wiring lands and can
                // decide what a Windows write should actually report.
                presenter.finish("done");
                outcome = Some(Ok(format!(
                    "{files_copied} files, {bytes_copied} bytes copied"
                )));
            }
            Event::Error { message, .. } => {
                presenter.abandon();
                outcome = Some(Err(ArgosError::Io(std::io::Error::other(message))));
            }
        }
    }
    outcome
}

/// Renders progress as an `indicatif` bar when stdout is a real, attended
/// terminal, and falls back to periodic plain-text status lines otherwise --
/// `indicatif` itself just draws nothing when stdout is piped/redirected
/// (backlog #16), which for a multi-gigabyte write leaves long stretches
/// with no way to tell "still working" from "hung" in any non-interactive
/// context (a log file, `tee`, a CI job). The underlying `Event` JSON stream
/// `argos-helper` emits never changes either way -- only this rendering
/// does.
enum Presenter {
    Bar(ProgressBar),
    Plain(PlainPresenter),
}

impl Presenter {
    fn new() -> Self {
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

/// Prints one line per phase change, plus periodic progress lines throttled
/// to roughly every 5 percentage points *or* every few seconds, whichever
/// comes first -- frequent enough that a long write never looks hung, rare
/// enough not to flood a log file.
struct PlainPresenter {
    current_phase: String,
    last_reported_percent: i64,
    last_reported_at: Instant,
    /// Set on every phase change, cleared on the next progress report --
    /// forces that first report to print unconditionally, rather than
    /// leaving it to `last_reported_percent`'s arithmetic (which only
    /// guarantees *a* report within the next 5 points, not necessarily on
    /// the very first one, if the new phase's first `Progress` event
    /// happens to land inside that window).
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

/// Shared with `write`'s confirmation prompt.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[unit])
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
}

fn locate_helper_binary() -> PathBuf {
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            let sibling = dir.join("argos-helper");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("argos-helper")
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_formats_bytes_and_larger_units() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(1536), "1.5KiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0GiB");
    }

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
