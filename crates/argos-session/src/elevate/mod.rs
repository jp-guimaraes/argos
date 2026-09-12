//! Locating `argos-helper`, elevating it, feeding it one `Plan`, and turning
//! its event stream into [`SessionEvent`]s and an [`Outcome`].
//!
//! Two elevators, behind one API. [`ElevationUi::Terminal`] is what `argos`
//! has always done -- `pkexec` where it exists on Linux, `sudo` otherwise,
//! talking over the child's own pipes. [`ElevationUi::Graphical`] is for a
//! front end with no controlling terminal, where `sudo` cannot ask for a
//! password at all; on macOS that means the system authorization dialog and
//! a pair of FIFOs standing in for those pipes (see [`graphical`]).
//!
//! The split into [`spawn`] and [`Running::stream`] is deliberate: the caller
//! holds a [`Canceller`] before it blocks on the stream, so cancellation is
//! something a button can do, not only a signal handler.

#[cfg(target_os = "macos")]
mod graphical;

use crate::cancel::Canceller;
use crate::events::{EventSink, Outcome, SessionEvent};
use argos_core::error::{ArgosError, Result};
use argos_privileged::protocol::{Event, Plan};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
// Only the macOS FIFO source needs a channel and a clock; Linux's graphical
// route is `pkexec`, an ordinary child with ordinary pipes.
#[cfg(target_os = "macos")]
use std::sync::mpsc::{Receiver, RecvTimeoutError};
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

/// How the user will be asked to authorize the write.
///
/// Not a cosmetic choice: under a windowed app there is no controlling
/// terminal, and `sudo` has nowhere to read a password from -- it fails
/// outright with "a terminal is required". Which elevator runs decides
/// whether the privileged side can start at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ElevationUi {
    /// A process with a terminal behind it. Unchanged from what `argos` has
    /// always done.
    #[default]
    Terminal,
    /// A process with no terminal: the desktop's own authorization dialog.
    Graphical,
}

/// How long to keep draining events after the elevated process has exited,
/// so anything it wrote just before exiting is not lost to the race.
#[cfg(target_os = "macos")]
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Which program was asked to elevate, so its exit status can be read the way
/// *that* program documents it.
///
/// Only `pkexec` gives the distinction any meaning: `pkexec(1)` promises
/// **126** when the user dismissed the authentication dialog and **127** when
/// the authorization could not be obtained for any other reason -- a wrong
/// password, or no authentication agent at all. Out of `sudo` or `osascript`
/// those same two numbers carry the shell's ordinary "could not execute"
/// meaning, so the mapping has to know which one ran rather than matching on
/// the number alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Elevator {
    Pkexec,
    /// `sudo` on either host, and macOS's `osascript` route, which reports a
    /// dismissed dialog in its stderr instead of in its exit status.
    Other,
}

/// An elevated `argos-helper` that has already been handed its `Plan`.
pub struct Running {
    events: EventStream,
    canceller: Canceller,
    child: Child,
    /// Set only for the graphical route, which cannot use the child's own
    /// pipes: its stderr is where AppleScript reports a dismissed dialog.
    stderr: Option<std::process::ChildStderr>,
    /// Which program was asked to elevate. Read only when the run settles
    /// nothing, to interpret the exit status it left behind.
    elevator: Elevator,
    /// Dropped last, removing the FIFO directory the graphical route needs.
    /// `None` for the terminal route, which has nothing to clean up.
    _rundir: Option<RunDir>,
}

impl Running {
    /// The handle a front end wires to Ctrl-C or to a Cancel button. Clone it
    /// before calling [`Running::stream`], which consumes `self`.
    pub fn canceller(&self) -> Canceller {
        self.canceller.clone()
    }

    /// Drains the helper's event stream into `sink` until it settles, then
    /// reaps the process.
    pub fn stream(mut self, sink: &mut dyn EventSink) -> Result<Outcome> {
        let outcome = stream_helper_events(&mut self.events, &mut self.child, sink);

        // The stream has ended, so there is nothing left to cancel: close the
        // channel now if a cancel didn't already, or the helper would sit
        // waiting on a pipe nobody is going to write to.
        self.canceller.close();
        self.events.shut_down();

        let status = self.child.wait()?;
        match outcome {
            Some(Ok(outcome)) => Ok(outcome),
            Some(Err(err)) => Err(err),
            None => Err(self.no_result_error(status)),
        }
    }

    /// The elevated process ended without ever settling an outcome. Usually
    /// that means elevation itself failed, and the useful thing to say is
    /// *why*.
    fn no_result_error(&mut self, status: std::process::ExitStatus) -> ArgosError {
        let mut stderr_text = String::new();
        if let Some(stderr) = self.stderr.as_mut() {
            let _ = stderr.read_to_string(&mut stderr_text);
        }
        no_result_error_from(
            self.elevator,
            &stderr_text,
            status.code(),
            status.success(),
            &status.to_string(),
        )
    }
}

/// Where event lines come from.
///
/// A pipe ends at EOF when the helper exits, which is all the terminal route
/// ever needed. A FIFO does not: the graphical route holds its own handle
/// open so the helper's `open()` cannot block, and that handle counts as a
/// writer, so the reader would wait forever for an EOF that only arrives
/// once *we* let go. Hence the explicit shutdown, and the child-exit poll
/// that ends the loop when the helper is gone.
enum EventStream {
    Pipe(std::io::Lines<BufReader<ChildStdout>>),
    /// macOS only. Linux's graphical route is `pkexec`, which is an ordinary
    /// child process with ordinary pipes -- the FIFO machinery exists purely
    /// because `do shell script` gives no pipes at all, so on Linux it would
    /// be dead code rather than an unused branch.
    #[cfg(target_os = "macos")]
    Fifo(FifoEvents),
}

#[cfg(target_os = "macos")]
struct FifoEvents {
    lines: Receiver<String>,
    /// Our own writer reference on the events FIFO. Dropping it is what
    /// finally lets the reader thread see EOF and finish.
    keepalive: Option<std::fs::File>,
    reader: Option<std::thread::JoinHandle<()>>,
    child_gone_since: Option<Instant>,
}

impl EventStream {
    /// The next event line, or `None` when there will not be another.
    ///
    /// `child` is consulted only by the FIFO source, which is macOS-only: a
    /// pipe ends at EOF on its own, so there is nothing to poll for.
    fn next_line(&mut self, child: &mut Child) -> Option<String> {
        let _ = &child;
        match self {
            EventStream::Pipe(lines) => lines.next().and_then(std::result::Result::ok),
            #[cfg(target_os = "macos")]
            EventStream::Fifo(fifo) => fifo.next_line(child),
        }
    }

    fn shut_down(&mut self) {
        #[cfg(target_os = "macos")]
        if let EventStream::Fifo(fifo) = self {
            // Drop our writer reference first: that is the EOF the reader
            // thread is waiting for, and without it the join below hangs.
            fifo.keepalive.take();
            if let Some(reader) = fifo.reader.take() {
                let _ = reader.join();
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl FifoEvents {
    fn next_line(&mut self, child: &mut Child) -> Option<String> {
        loop {
            match self.lines.recv_timeout(Duration::from_millis(50)) {
                Ok(line) => return Some(line),
                Err(RecvTimeoutError::Disconnected) => return None,
                Err(RecvTimeoutError::Timeout) => {}
            }

            // The escape hatch a blocking read does not have. If the user
            // dismisses the authorization dialog, the elevated process exits
            // without ever opening the other end, and nothing will ever
            // arrive on this channel.
            if self.child_gone_since.is_none() && matches!(child.try_wait(), Ok(Some(_))) {
                self.child_gone_since = Some(Instant::now());
            }
            if let Some(since) = self.child_gone_since {
                if since.elapsed() >= DRAIN_GRACE {
                    // One last look, so an event written just before the
                    // process exited is not lost to the race.
                    return self.lines.try_recv().ok();
                }
            }
        }
    }
}

/// A temporary directory that removes itself, holding the FIFOs the macOS
/// graphical route talks over. The `Plan` never exists as a file -- it only
/// ever transits a FIFO -- so even a leaked directory leaks nothing.
struct RunDir(PathBuf);

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Elevates and hands `plan` to `argos-helper`, returning as soon as it is
/// running -- before any of its work is done.
pub fn spawn(plan: &Plan, ui: ElevationUi) -> Result<Running> {
    let helper_path = locate_helper_binary();
    match ui {
        ElevationUi::Terminal => spawn_terminal(plan, &helper_path),
        ElevationUi::Graphical => spawn_graphical(plan, &helper_path),
    }
}

/// What `argos` has always done, unchanged: `pkexec` where it exists on
/// Linux, `sudo` otherwise, over the child's own pipes.
fn spawn_terminal(plan: &Plan, helper_path: &Path) -> Result<Running> {
    let elevator = if cfg!(target_os = "linux") && command_exists("pkexec") {
        Elevator::Pkexec
    } else {
        Elevator::Other
    };
    let elevation_command = match elevator {
        Elevator::Pkexec => "pkexec",
        Elevator::Other => "sudo",
    };

    let mut child = Command::new(elevation_command)
        .arg(helper_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    {
        // Newline-terminated: the helper reads one *line* and keeps the rest
        // of the pipe open for the cancel byte, rather than reading to EOF.
        let plan_json = serde_json::to_string(plan).map_err(std::io::Error::other)?;
        writeln!(stdin, "{plan_json}")?;
        stdin.flush()?;
    }

    let stdout = child.stdout.take().expect("stdout was piped");
    Ok(Running {
        events: EventStream::Pipe(BufReader::new(stdout).lines()),
        // Held open, not dropped: this is what both delivers the cancel byte
        // the helper's watcher thread is reading for and, when finally
        // closed, gives it the EOF `protocol::watch_for_cancel` treats as a
        // second, redundant signal.
        canceller: Canceller::new(Box::new(stdin)),
        child,
        stderr: None,
        elevator,
        _rundir: None,
    })
}

#[cfg(target_os = "macos")]
fn spawn_graphical(plan: &Plan, helper_path: &Path) -> Result<Running> {
    graphical::spawn(plan, helper_path)
}

/// On Linux the graphical route is `pkexec`, which renders the desktop's own
/// polkit agent -- the same command the terminal route already prefers, just
/// without a `sudo` fallback that would have no terminal to fall back to.
#[cfg(target_os = "linux")]
fn spawn_graphical(plan: &Plan, helper_path: &Path) -> Result<Running> {
    if !command_exists("pkexec") {
        return Err(ArgosError::Io(std::io::Error::other(
            "pkexec is not installed, and a graphical session has no terminal for sudo to \
             ask for a password in; install polkit, or run `argos write` from a terminal",
        )));
    }

    // Deliberately *not* `--disable-internal-agent`. With no desktop agent
    // registered, pkexec falls back to a textual one: from a window that has
    // no controlling terminal it cannot ask anything and exits 127, which
    // `no_result_error_from` turns into a sentence naming the missing agent;
    // and a build launched from a terminal during development still gets a
    // usable prompt instead of a refusal. Disabling it would only replace the
    // second case with the first.
    let mut child = Command::new("pkexec")
        .arg(helper_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Captured, not inherited, for the same reason the macOS route does
        // it: this is where pkexec explains a 127, and a window has nowhere
        // to show a message it let escape to a terminal that isn't there.
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    {
        let plan_json = serde_json::to_string(plan).map_err(std::io::Error::other)?;
        writeln!(stdin, "{plan_json}")?;
        stdin.flush()?;
    }

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take();
    Ok(Running {
        events: EventStream::Pipe(BufReader::new(stdout).lines()),
        canceller: Canceller::new(Box::new(stdin)),
        child,
        stderr,
        elevator: Elevator::Pkexec,
        _rundir: None,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn spawn_graphical(_plan: &Plan, _helper_path: &Path) -> Result<Running> {
    Err(ArgosError::NotImplemented(
        "graphical elevation on this platform",
    ))
}

/// Reads one JSON [`Event`] per line, driving `sink`, until a terminal event
/// settles the outcome (or the stream simply ends).
///
/// Takes the sink rather than owning a presenter, so it is testable against
/// an in-memory buffer -- which the old version, wired straight to a
/// `ProgressBar`, was not.
fn stream_helper_events(
    events: &mut EventStream,
    child: &mut Child,
    sink: &mut dyn EventSink,
) -> Option<std::result::Result<Outcome, ArgosError>> {
    let mut outcome = None;
    while let Some(line) = events.next_line(child) {
        let Ok(event) = serde_json::from_str::<Event>(&line) else {
            continue;
        };
        match event {
            Event::Phase { phase } => sink.on_event(SessionEvent::Phase(phase)),
            Event::Progress {
                bytes_done,
                bytes_total,
            } => sink.on_event(SessionEvent::Progress {
                bytes_done,
                bytes_total,
            }),
            Event::Done { written_hash } => {
                let settled = Outcome::DdWrite { hash: written_hash };
                sink.on_finished(&settled);
                outcome = Some(Ok(settled));
            }
            Event::VerifyOk { hash } => {
                let settled = Outcome::Verify { hash };
                sink.on_finished(&settled);
                outcome = Some(Ok(settled));
            }
            Event::WindowsDone {
                files_copied,
                bytes_copied,
            } => {
                let settled = Outcome::WindowsWrite {
                    files_copied,
                    bytes_copied,
                };
                sink.on_finished(&settled);
                outcome = Some(Ok(settled));
            }
            Event::WindowsVerifyOk { files_verified } => {
                let settled = Outcome::WindowsVerify { files_verified };
                sink.on_finished(&settled);
                outcome = Some(Ok(settled));
            }
            Event::Ejected { device_path, error } => {
                sink.on_event(SessionEvent::Ejected { device_path, error })
            }
            Event::Error { message, exit_code } => {
                sink.on_failed();
                outcome = Some(Err(ArgosError::Helper { message, exit_code }));
            }
        }
    }
    outcome
}

/// Turns "the elevated process ended and reported nothing" into the most
/// specific error the evidence supports.
///
/// Split out from [`Running::no_result_error`] and taking plain values rather
/// than an `ExitStatus`, so every branch is reachable in a test -- the
/// interesting one otherwise needs a human to dismiss an authorization
/// dialog, which is exactly the kind of thing that goes unverified.
fn no_result_error_from(
    elevator: Elevator,
    stderr: &str,
    code: Option<i32>,
    succeeded: bool,
    status_text: &str,
) -> ArgosError {
    // AppleScript's "User canceled". The dialog was dismissed, so nothing was
    // elevated and nothing was touched.
    if stderr.contains("-128") {
        return ArgosError::ElevationDeclined;
    }

    // pkexec's equivalent, which it signals in the exit status and nowhere
    // else: a dismissed polkit dialog prints *nothing at all* on stderr, so
    // without this the user who clicked Cancel was told "argos-helper exited
    // with exit status: 126 and reported no result".
    if elevator == Elevator::Pkexec {
        match code {
            Some(126) => return ArgosError::ElevationDeclined,
            // "not authorized, or an error occurred" -- the wrong password
            // three times, and equally the failure mode this project has to
            // name out loud: a window launched from a .desktop file on a
            // desktop with no polkit agent running, where pkexec has no
            // terminal to fall back to either.
            Some(127) => {
                let detail = stderr.trim();
                let detail = if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                };
                return ArgosError::Io(std::io::Error::other(format!(
                    "could not obtain authorization to run argos-helper{detail}; if no \
                     authentication agent is running on this desktop, start one or run \
                     `argos write` from a terminal"
                )));
            }
            _ => {}
        }
    }

    if !stderr.trim().is_empty() {
        return ArgosError::Io(std::io::Error::other(format!(
            "could not elevate argos-helper: {}",
            stderr.trim()
        )));
    }
    if succeeded {
        ArgosError::Io(std::io::Error::other(
            "argos-helper exited successfully but reported no result",
        ))
    } else {
        ArgosError::Io(std::io::Error::other(format!(
            "argos-helper exited with {status_text} and reported no result"
        )))
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
    use argos_core::progress::Phase;
    use argos_privileged::protocol::PhaseWire;

    #[derive(Default)]
    struct Recorder {
        events: Vec<SessionEvent>,
        finished: Vec<Outcome>,
        failed: usize,
    }
    impl EventSink for Recorder {
        fn on_event(&mut self, event: SessionEvent) {
            self.events.push(event);
        }
        fn on_finished(&mut self, outcome: &Outcome) {
            self.finished.push(outcome.clone());
        }
        fn on_failed(&mut self) {
            self.failed += 1;
        }
    }

    /// Drives the event loop over canned text.
    ///
    /// `EventStream::Pipe` holds a `ChildStdout`, which cannot be built from
    /// a buffer, so the text goes through a real `cat` -- which doubles as
    /// the already-finished child the loop polls. Hermetic: no elevation, no
    /// helper, no device.
    fn drain(lines: &str) -> (Recorder, Option<std::result::Result<Outcome, ArgosError>>) {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn cat");
        let mut stdin = child.stdin.take().expect("piped");
        stdin.write_all(lines.as_bytes()).expect("feed cat");
        drop(stdin);

        let mut stream =
            EventStream::Pipe(BufReader::new(child.stdout.take().expect("piped")).lines());
        let mut sink = Recorder::default();
        let outcome = stream_helper_events(&mut stream, &mut child, &mut sink);
        child.wait().expect("reap cat");
        (sink, outcome)
    }

    #[test]
    fn progress_reaches_the_sink_in_order_and_settles_on_done() {
        let (sink, outcome) = drain(concat!(
            // The wire form a current helper emits: snake_case, typed.
            r#"{"event":"phase","phase":"writing"}"#,
            "\n",
            r#"{"event":"progress","bytes_done":1,"bytes_total":4}"#,
            "\n",
            r#"{"event":"done","written_hash":"abc"}"#,
            "\n",
        ));
        assert_eq!(
            sink.events,
            vec![
                SessionEvent::Phase(PhaseWire::Known(Phase::Writing)),
                SessionEvent::Progress {
                    bytes_done: 1,
                    bytes_total: 4
                },
            ]
        );
        assert_eq!(sink.failed, 0);
        assert!(matches!(outcome, Some(Ok(Outcome::DdWrite { hash })) if hash == "abc"));
    }

    /// A line the parser cannot read is skipped rather than ending the run --
    /// the behaviour that lets a mixed-version pair degrade instead of
    /// crashing.
    #[test]
    fn an_unparseable_line_is_skipped_not_fatal() {
        let (sink, outcome) = drain(concat!(
            "not json at all\n",
            r#"{"event":"unknown_kind"}"#,
            "\n",
            r#"{"event":"verify_ok","hash":"def"}"#,
            "\n",
        ));
        assert!(sink.events.is_empty());
        assert!(matches!(outcome, Some(Ok(Outcome::Verify { hash })) if hash == "def"));
    }

    #[test]
    fn an_error_event_fails_the_sink_and_the_outcome() {
        let (sink, outcome) = drain(concat!(
            r#"{"event":"error","message":"device is gone","exit_code":10}"#,
            "\n",
        ));
        assert_eq!(sink.failed, 1);
        let err = outcome.expect("settled").expect_err("must be an error");
        assert!(err.to_string().contains("device is gone"));
    }

    /// The defect this milestone exists to fix: the helper's own code used to
    /// be dropped on the floor here, so a system disk (12), a checksum
    /// mismatch (17) and a cancelled write (18) all exited 19.
    #[test]
    fn the_helpers_own_exit_code_survives_the_boundary() {
        for (message, code) in [("system disk", 12), ("checksum", 17), ("cancelled", 18)] {
            let line =
                format!(r#"{{"event":"error","message":"{message}","exit_code":{code}}}"#) + "\n";
            let (_, outcome) = drain(&line);
            let err = outcome.expect("settled").expect_err("must be an error");
            assert_eq!(err.exit_code(), code, "for {message}");
            // The text a user sees is unchanged; only the code is now true.
            assert_eq!(err.to_string(), message);
        }
    }

    /// An old helper's `Debug`-formatted phase still reaches the sink rather
    /// than being dropped, just without a type to match on.
    #[test]
    fn a_phase_from_an_older_helper_degrades_instead_of_vanishing() {
        let (sink, _) = drain(concat!(
            r#"{"event":"phase","phase":"Writing"}"#,
            "\n",
            r#"{"event":"phase","phase":"writing"}"#,
            "\n",
        ));
        assert_eq!(
            sink.events,
            vec![
                SessionEvent::Phase(PhaseWire::Unknown("Writing".into())),
                SessionEvent::Phase(PhaseWire::Known(Phase::Writing)),
            ]
        );
    }

    #[test]
    fn windows_outcomes_carry_their_numbers_rather_than_a_sentence() {
        let (sink, outcome) = drain(concat!(
            r#"{"event":"windows_done","files_copied":905,"bytes_copied":2048}"#,
            "\n",
        ));
        assert_eq!(
            sink.finished,
            vec![Outcome::WindowsWrite {
                files_copied: 905,
                bytes_copied: 2048
            }]
        );
        assert!(matches!(
            outcome,
            Some(Ok(Outcome::WindowsWrite {
                files_copied: 905,
                ..
            }))
        ));
    }

    /// The eject report arrives after the terminal event and must not
    /// disturb the outcome that is already settled.
    #[test]
    fn an_eject_after_the_terminal_event_leaves_the_outcome_alone() {
        let (sink, outcome) = drain(concat!(
            r#"{"event":"done","written_hash":"abc"}"#,
            "\n",
            r#"{"event":"ejected","device_path":"/dev/sdz","error":null}"#,
            "\n",
        ));
        assert_eq!(
            sink.events,
            vec![SessionEvent::Ejected {
                device_path: "/dev/sdz".into(),
                error: None
            }]
        );
        assert!(matches!(outcome, Some(Ok(Outcome::DdWrite { hash })) if hash == "abc"));
    }

    #[test]
    fn a_failed_eject_is_reported_but_still_not_an_error() {
        let (sink, outcome) = drain(concat!(
            r#"{"event":"done","written_hash":"abc"}"#,
            "\n",
            r#"{"event":"ejected","device_path":"/dev/sdz","error":"busy"}"#,
            "\n",
        ));
        assert_eq!(
            sink.events,
            vec![SessionEvent::Ejected {
                device_path: "/dev/sdz".into(),
                error: Some("busy".into())
            }]
        );
        assert!(outcome.expect("settled").is_ok());
    }

    #[test]
    fn a_stream_that_ends_without_a_terminal_event_settles_nothing() {
        let (_, outcome) = drain(concat!(
            r#"{"event":"progress","bytes_done":1,"bytes_total":4}"#,
            "\n",
        ));
        assert!(outcome.is_none());
    }

    /// Dismissing the authorization dialog is not a failure to report -- it
    /// is the user declining, before anything was touched.
    ///
    /// It must not borrow `NotConfirmed`'s wording, which is about the typed
    /// device path: someone who cancelled an authorization prompt and was
    /// told their "confirmation did not match" would reasonably go looking
    /// for a typo that was never there. Found by a human actually clicking
    /// Cancel, after the mapping itself was already correct.
    #[test]
    fn a_dismissed_authorization_dialog_says_authorization_not_confirmation() {
        let err = no_result_error_from(
            Elevator::Other,
            "22:256: execution error: User canceled. (-128)",
            Some(1),
            false,
            "exit status: 1",
        );
        assert!(matches!(err, ArgosError::ElevationDeclined));
        assert_eq!(err.exit_code(), 27, "same meaning as NotConfirmed");
        let message = err.to_string();
        assert!(message.contains("authorization"), "{message}");
        assert!(!message.contains("confirmation did not match"), "{message}");
    }

    #[test]
    fn any_other_elevation_failure_says_what_it_was() {
        let err = no_result_error_from(
            Elevator::Other,
            "sudo: a terminal is required to read the password",
            Some(1),
            false,
            "exit status: 1",
        );
        let message = err.to_string();
        assert!(
            message.contains("could not elevate argos-helper"),
            "{message}"
        );
        assert!(
            message.contains("a terminal is required to read the password"),
            "{message}"
        );
    }

    /// 126 is what `pkexec(1)` promises for "the user dismissed the
    /// authentication dialog". Both spellings below came from a human
    /// actually clicking Cancel on GNOME 46/X11 (Ubuntu 24.04, polkit 124):
    /// exit 126, with pkexec also printing *Error executing command as
    /// another user: Request dismissed*.
    ///
    /// That printed line is why the exit code is consulted **before** the
    /// stderr branch further down rather than after it. The graphical route
    /// captures stderr, so a dismissal reaching that branch would come back
    /// as "could not elevate argos-helper: Error executing command as another
    /// user: Request dismissed" -- an I/O failure exiting 19, where the user
    /// simply declined and nothing was touched (27). The terminal route
    /// inherits stderr rather than capturing it, so there the exit status is
    /// the only evidence there is.
    ///
    /// Before either, someone who clicked Cancel was told "argos-helper
    /// exited with exit status: 126 and reported no result", which names the
    /// wrong program and reads like a crash.
    #[test]
    fn a_dismissed_polkit_dialog_is_a_declined_authorization() {
        for stderr in [
            // The graphical route, which pipes stderr.
            "Error executing command as another user: Request dismissed\n",
            // The terminal route, which does not.
            "",
        ] {
            let err = no_result_error_from(
                Elevator::Pkexec,
                stderr,
                Some(126),
                false,
                "exit status: 126",
            );
            assert!(
                matches!(err, ArgosError::ElevationDeclined),
                "stderr {stderr:?} was not read as a dismissed dialog"
            );
            assert_eq!(err.exit_code(), 27, "same meaning as NotConfirmed");
        }
    }

    /// `pkexec(1)`'s other authorization failure: "not authorized, or an
    /// error occurred". It covers three wrong passwords and, the reason a
    /// window needs it named, a desktop running no authentication agent at
    /// all -- where pkexec has no TTY to fall back to either.
    ///
    /// Unlike the 126 above, this one is **not** backed by a run on real
    /// hardware: reproducing it needs a host with no polkit agent, and on a
    /// systemd/GNOME box every attempt to arrange one still resolved to the
    /// graphical session and put its dialog back on screen -- a transient
    /// `systemd-run --user` service included, whose polkit subject logs as
    /// `unix-process:<systemd --user>` *in* that session. The mapping follows
    /// what pkexec documents; whether an agentless pkexec exits 127 or simply
    /// blocks is worth confirming on such a host before a front end relies on
    /// seeing this error rather than hanging.
    #[test]
    fn pkexec_127_names_the_authentication_agent() {
        let err = no_result_error_from(Elevator::Pkexec, "", Some(127), false, "exit status: 127");
        let message = err.to_string();
        assert!(
            message.contains("could not obtain authorization"),
            "{message}"
        );
        assert!(message.contains("authentication agent"), "{message}");
        assert!(message.contains("from a terminal"), "{message}");
    }

    /// When stderr *was* captured -- the graphical route pipes it -- pkexec's
    /// own explanation is kept, rather than being replaced by the guess.
    #[test]
    fn pkexec_127_keeps_what_pkexec_itself_said() {
        let err = no_result_error_from(
            Elevator::Pkexec,
            "Error executing command as another user: Not authorized\n",
            Some(127),
            false,
            "exit status: 127",
        );
        let message = err.to_string();
        assert!(message.contains("Not authorized"), "{message}");
        assert!(message.contains("authentication agent"), "{message}");
    }

    /// 126 and 127 mean "could not execute" out of a shell, and `sudo` passes
    /// a command's own status straight through. Reading either as "the user
    /// declined" because the number matched would turn a real failure into a
    /// silent, successful-looking abort.
    #[test]
    fn the_same_codes_out_of_sudo_are_not_a_declined_authorization() {
        for code in [126, 127] {
            let err = no_result_error_from(
                Elevator::Other,
                "",
                Some(code),
                false,
                &format!("exit status: {code}"),
            );
            assert!(
                !matches!(err, ArgosError::ElevationDeclined),
                "sudo exit {code} was read as a dismissed dialog"
            );
            assert_eq!(
                err.to_string(),
                format!("argos-helper exited with exit status: {code} and reported no result")
            );
        }
    }

    /// The two messages the terminal route has always produced, unchanged --
    /// it captures no stderr, so it always lands here.
    #[test]
    fn a_silent_exit_keeps_the_wording_the_cli_has_always_printed() {
        assert_eq!(
            no_result_error_from(Elevator::Pkexec, "", Some(0), true, "exit status: 0").to_string(),
            "argos-helper exited successfully but reported no result"
        );
        assert_eq!(
            no_result_error_from(Elevator::Pkexec, "", Some(1), false, "exit status: 1")
                .to_string(),
            "argos-helper exited with exit status: 1 and reported no result"
        );
    }

    #[test]
    fn the_default_elevation_is_the_terminal_one() {
        assert_eq!(ElevationUi::default(), ElevationUi::Terminal);
    }
}
