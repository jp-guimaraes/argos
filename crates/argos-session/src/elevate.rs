//! Locating `argos-helper`, elevating it, feeding it one `Plan`, and turning
//! its event stream into [`SessionEvent`]s and an [`Outcome`].
//!
//! Moved out of `argos-cli`'s private `commands::helper` so both the CLI and
//! a GUI drive one implementation. The split into [`spawn`] and
//! [`Running::stream`] is the one shape change: the caller now holds a
//! [`Canceller`] before it blocks on the stream, so cancellation no longer
//! has to be a signal handler reaching into a mutex.

use crate::cancel::Canceller;
use crate::events::{EventSink, Outcome, SessionEvent};
use argos_core::error::{ArgosError, Result};
use argos_privileged::protocol::{Event, Plan};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};

/// An elevated `argos-helper` that has already been handed its `Plan`.
pub struct Running {
    events: BufReader<ChildStdout>,
    canceller: Canceller,
    child: Child,
}

impl Running {
    /// The handle a front end wires to Ctrl-C or to a Cancel button. Clone it
    /// before calling [`Running::stream`], which consumes `self`.
    pub fn canceller(&self) -> Canceller {
        self.canceller.clone()
    }
}

/// Elevates and hands `plan` to `argos-helper`, returning as soon as it is
/// running -- before any of its work is done.
pub fn spawn(plan: &Plan) -> Result<Running> {
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
        // Newline-terminated: the helper reads one *line* and keeps the rest
        // of the pipe open for the cancel byte, rather than reading to EOF.
        let plan_json = serde_json::to_string(plan).map_err(std::io::Error::other)?;
        writeln!(stdin, "{plan_json}")?;
        stdin.flush()?;
    }

    let stdout = child.stdout.take().expect("stdout was piped");
    Ok(Running {
        events: BufReader::new(stdout),
        // Held open, not dropped: this is what both delivers the cancel byte
        // the helper's watcher thread is reading for and, when finally
        // closed, gives it the EOF `protocol::watch_for_cancel` treats as a
        // second, redundant signal.
        canceller: Canceller::new(Box::new(stdin)),
        child,
    })
}

impl Running {
    /// Drains the helper's event stream into `sink` until it settles, then
    /// reaps the process.
    pub fn stream(mut self, sink: &mut dyn EventSink) -> Result<Outcome> {
        let outcome = stream_helper_events(&mut self.events, sink);

        // The stream has ended, so there is nothing left to cancel: close the
        // channel now if a cancel didn't already, or the helper would sit
        // waiting on a pipe nobody is going to write to.
        self.canceller.close();

        let status = self.child.wait()?;
        match outcome {
            Some(Ok(outcome)) => Ok(outcome),
            Some(Err(err)) => Err(err),
            None if status.success() => Err(ArgosError::Io(std::io::Error::other(
                "argos-helper exited successfully but reported no result",
            ))),
            None => Err(ArgosError::Io(std::io::Error::other(format!(
                "argos-helper exited with {status} and reported no result"
            )))),
        }
    }
}

/// Reads one JSON [`Event`] per line, driving `sink`, until a terminal event
/// settles the outcome (or the stream simply ends).
///
/// Generic over the reader, and taking the sink rather than owning a
/// presenter, so it is testable against an in-memory buffer -- which the old
/// version, wired straight to a `ProgressBar`, was not.
fn stream_helper_events<R: BufRead>(
    reader: &mut R,
    sink: &mut dyn EventSink,
) -> Option<std::result::Result<Outcome, ArgosError>> {
    let mut outcome = None;
    for line in reader.lines().map_while(std::result::Result::ok) {
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
            Event::Error { message, .. } => {
                sink.on_failed();
                // The helper's own `exit_code` is discarded here, so every
                // helper-side failure surfaces as 19. That is a real defect,
                // and it is deliberately left alone in this refactor: fixing
                // it changes a user-visible exit code, so it belongs to its
                // own change (#89) rather than hiding inside a move.
                outcome = Some(Err(ArgosError::Io(std::io::Error::other(message))));
            }
        }
    }
    outcome
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
    use std::io::Cursor;

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

    fn drain(lines: &str) -> (Recorder, Option<std::result::Result<Outcome, ArgosError>>) {
        let mut sink = Recorder::default();
        let mut reader = Cursor::new(lines.as_bytes().to_vec());
        let outcome = stream_helper_events(&mut reader, &mut sink);
        (sink, outcome)
    }

    #[test]
    fn progress_reaches_the_sink_in_order_and_settles_on_done() {
        let (sink, outcome) = drain(concat!(
            r#"{"event":"phase","phase":"Writing"}"#,
            "\n",
            r#"{"event":"progress","bytes_done":1,"bytes_total":4}"#,
            "\n",
            r#"{"event":"done","written_hash":"abc"}"#,
            "\n",
        ));
        assert_eq!(
            sink.events,
            vec![
                SessionEvent::Phase("Writing".into()),
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

    /// Pins the defect deliberately left in place by this refactor, so #89
    /// has something to change and nobody "fixes" it here by accident.
    #[test]
    fn the_helpers_exit_code_is_still_discarded_pending_issue_89() {
        let (_, outcome) = drain(concat!(
            r#"{"event":"error","message":"system disk","exit_code":12}"#,
            "\n",
        ));
        let err = outcome.expect("settled").expect_err("must be an error");
        assert_eq!(err.exit_code(), 19, "still Io(..) until #89 lands");
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
}
