//! Shared machinery for talking to the privileged `argos-helper` process:
//! locating the binary, elevating (`pkexec`/`sudo`), feeding it a `Plan` as
//! JSON on stdin, and turning its `Event` stream into human-visible
//! progress. Used by `write` and `verify` for both the DD-mode plans
//! (`Plan::Write`/`Plan::Verify`) and the Windows installer ones
//! (`Plan::WriteWindowsImage`/`Plan::VerifyWindowsImage`) -- the only
//! difference between them, from here, is which terminal event carries the
//! summary string each command prints.

use argos_core::error::{ArgosError, Result};
use argos_privileged::protocol::{self, Event, Plan};
use indicatif::{ProgressBar, ProgressStyle};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Resolves an ISO path to absolute before it's put in a `Plan` and sent
/// across the privilege boundary to `argos-helper`.
///
/// Load-bearing, not defensive: `argos-helper` opens `plan.image_path` (or
/// `plan.iso_path`) relative to *its own* working directory, not the shell's
/// the user typed a relative path in. `sudo` commonly preserves the caller's
/// cwd, which is why this went unnoticed for a long time -- `pkexec`
/// deliberately does not (the same cwd-reset hardening every setuid-style
/// launcher does, to stop a relative path from resolving somewhere the
/// caller didn't intend), so it surfaces exactly there: confirmed on real
/// hardware, running `argos write some.iso --device /dev/sdg` from the
/// directory holding the ISO -- unmounting succeeds, then `File::open` on
/// the (still-relative) path fails with a bare, pathless "No such file or
/// directory" from deep inside the elevated helper, well after the
/// destructive confirmation prompt.
///
/// Resolving here instead means a bad path fails immediately, with the path
/// named in the message, before any confirmation prompt, unmount, or
/// elevation -- not moments after the user has confirmed a write.
pub fn canonicalize_iso_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .map_err(|err| ArgosError::Io(std::io::Error::other(format!("{}: {err}", path.display()))))
}

/// Elevates, feeds `plan` to `argos-helper`, renders its progress, and
/// returns the hash it reported on success (`Event::Done`'s `written_hash`
/// for a write, `Event::VerifyOk`'s `hash` for a verify -- callers already
/// know which one they sent, so they don't need the `Event` back, only the
/// string).
///
/// Cancellation (backlog #35): the helper's stdin is *not* closed right after
/// the `Plan` line -- it stays open, held here behind a mutex, for the rest of
/// the call. A `SIGINT` handler (`ctrlc`) writes [`protocol::CANCEL_SIGNAL`]
/// into it and drops it, which both delivers the byte `argos-helper`'s watcher
/// thread is reading for and closes the pipe as a second, redundant signal in
/// case the byte doesn't make it.
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

    let mut stdin = child.stdin.take().expect("stdin was piped");
    {
        // Newline-terminated now: the helper reads one *line* and keeps the
        // rest of the pipe open for the cancel byte, rather than reading to
        // EOF.
        let plan_json = serde_json::to_string(plan).map_err(std::io::Error::other)?;
        writeln!(stdin, "{plan_json}")?;
        stdin.flush()?;
    }

    // Held open, not dropped -- see this function's doc comment.
    let stdin = Arc::new(Mutex::new(Some(stdin)));
    let stdin_for_handler = Arc::clone(&stdin);
    // Best-effort: a second `run_plan` in the same process (doesn't happen
    // today) would find the handler already registered and get an `Err` here,
    // which is fine to ignore -- the first registration already covers this
    // process's whole lifetime.
    let _ = ctrlc::set_handler(move || {
        if let Ok(mut guard) = stdin_for_handler.lock() {
            if let Some(mut stdin) = guard.take() {
                // Best-effort: if the helper has already exited this just
                // fails with a broken pipe, and there is nothing left to
                // cancel anyway. Dropping the handle right after closes the
                // pipe, which is itself a second, redundant cancel signal
                // (see `protocol::watch_for_cancel`).
                let _ = stdin.write_all(&[protocol::CANCEL_SIGNAL]);
                let _ = stdin.flush();
            }
        }
    });

    let stdout = child.stdout.take().expect("stdout was piped");
    let outcome = stream_helper_events(BufReader::new(stdout));

    // The event stream has ended, so there is nothing left to cancel: close
    // stdin now if the handler above didn't already, or the helper would sit
    // waiting on a pipe nobody is going to write to.
    if let Ok(mut guard) = stdin.lock() {
        guard.take();
    }

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
                // really fit a two-partition write -- there's no one
                // meaningful whole-device hash for it (see
                // `argos_privileged::windows::WindowsWriteOutcome`'s doc
                // comment), so this reports a file/byte summary instead;
                // `write::run_windows_write` is what prints it.
                presenter.finish("done");
                outcome = Some(Ok(format!(
                    "{files_copied} files copied ({})",
                    human_size(bytes_copied)
                )));
            }
            Event::WindowsVerifyOk { files_verified } => {
                presenter.finish("verified");
                outcome = Some(Ok(format!("{files_verified} files verified")));
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

    /// This is the actual bug: a relative path resolves fine right here
    /// (same process, same cwd as the shell), which is exactly what let it
    /// through code review and every existing test (all of which use
    /// tempfile's always-absolute paths) -- and then fails deep inside
    /// argos-helper, elevated via a mechanism that may not preserve cwd,
    /// with an error naming no path at all. Pins the fix at the type that
    /// actually crosses the privilege boundary: what canonicalize_iso_path
    /// returns must be absolute, not merely "resolved without error".
    #[test]
    fn a_relative_path_comes_back_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("some.iso");
        std::fs::write(&iso, b"x").unwrap();

        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let result = canonicalize_iso_path(Path::new("some.iso"));
        std::env::set_current_dir(original_cwd).unwrap();

        let resolved = result.expect("a file that exists should resolve");
        assert!(resolved.is_absolute(), "got {resolved:?}, not absolute");
        assert_eq!(resolved.file_name().unwrap(), "some.iso");
    }

    /// The failure case has to name the path -- a bare "No such file or
    /// directory" is what the user actually saw in the field, and it names
    /// nothing to act on.
    #[test]
    fn a_missing_path_is_named_in_the_error() {
        let err = canonicalize_iso_path(Path::new("/definitely/does/not/exist.iso"))
            .expect_err("a nonexistent path must not resolve");
        let message = err.to_string();
        assert!(
            message.contains("does/not/exist.iso"),
            "error {message:?} does not name the path"
        );
    }

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
