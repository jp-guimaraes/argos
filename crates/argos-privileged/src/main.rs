//! `argos-helper`: the one binary in this project meant to run as root.
//!
//! Deliberately "dumb" by design (backlog E7): reads a single, already
//! user-confirmed `protocol::Plan` from stdin, re-validates the target
//! device against its *current* state (never trusts the plan blindly --
//! `protocol::validate_refreshed_device` is the TOCTOU guard for a write;
//! see `protocol::VerifyPlan`'s doc comment for why a standalone verify
//! doesn't need the same one), then either writes the image (optionally
//! verifying it) or just verifies, and exits. It parses no ISO, talks to no
//! D-Bus/plist/UDisks2 API, and never lingers waiting for a second command.
//! The actual logic lives in `lib.rs::execute`/`execute_verify` -- this file
//! is only stdin/stdout JSON framing and dispatch around them.
//!
//! Cancellation (backlog #35): once the `Plan` line is read, whatever's left
//! of stdin becomes a one-byte control channel -- see
//! `protocol::watch_for_cancel`'s doc comment. A background thread watches
//! it for the rest of this process's life, cancelling the `CancelToken`
//! passed to whichever write path is running (verify ignores it: it's
//! read-only, nothing to cancel mid-operation that leaves anything
//! inconsistent).

use argos_core::progress::{CancelToken, Phase, ProgressSink};
use argos_privileged::protocol::{self, Event, Plan};
use std::io::{Stdin, Write};

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    // Deliberately the unlocked `Stdin` handle, not `.lock()`'s
    // `StdinLock`: `StdinLock` holds an actual `MutexGuard` for as long as
    // it's alive, which isn't `Send`, so it can't be handed to the watcher
    // thread spawned below. `Stdin` itself is `Send + Sync` (each of its
    // `Read`/`read_line` calls takes the lock internally, only for that
    // call), so `read_plan` borrows it just for the first line and the same
    // handle is then moved into the thread for the rest of the stream.
    let stdin = std::io::stdin();
    let plan = match read_plan(&stdin) {
        Ok(plan) => plan,
        Err(err) => {
            emit(&Event::Error {
                message: format!("failed to read plan: {err}"),
                exit_code: 1,
            });
            return 1;
        }
    };

    let cancel = CancelToken::new();
    let cancel_for_watcher = cancel.clone();
    std::thread::spawn(move || protocol::watch_for_cancel(stdin, cancel_for_watcher));

    let result = match plan {
        Plan::Write(write_plan) => argos_privileged::execute(&write_plan, &JsonlProgress, &cancel)
            .map(|written_hash| Event::Done { written_hash }),
        Plan::Verify(verify_plan) => argos_privileged::execute_verify(&verify_plan, &JsonlProgress)
            .map(|hash| Event::VerifyOk { hash }),
        Plan::WriteWindowsImage(windows_plan) => {
            argos_privileged::windows::execute_write_windows_image(
                &windows_plan,
                &JsonlProgress,
                &cancel,
            )
            .map(|outcome| Event::WindowsDone {
                files_copied: outcome.files_copied,
                bytes_copied: outcome.bytes_copied,
            })
        }
        Plan::VerifyWindowsImage(verify_windows_plan) => {
            argos_privileged::windows::execute_verify_windows_image(
                &verify_windows_plan,
                &JsonlProgress,
            )
            .map(|outcome| Event::WindowsVerifyOk {
                files_verified: outcome.files_verified,
            })
        }
    };

    match result {
        Ok(event) => {
            emit(&event);
            0
        }
        Err(err) => {
            let exit_code = err.exit_code();
            emit(&Event::Error {
                message: err.to_string(),
                exit_code,
            });
            exit_code
        }
    }
}

/// Reads exactly the first line of stdin as the [`Plan`] -- not the whole
/// stream (that's the change backlog #35 made here: the rest of stdin is
/// left for the caller to hand to `protocol::watch_for_cancel` on its own
/// thread, since `argos` no longer closes its end of the pipe right after
/// sending this line).
fn read_plan(stdin: &Stdin) -> std::io::Result<Plan> {
    let mut line = String::new();
    stdin.read_line(&mut line)?;
    serde_json::from_str(line.trim_end()).map_err(std::io::Error::other)
}

struct JsonlProgress;

impl ProgressSink for JsonlProgress {
    fn on_phase(&self, phase: Phase) {
        emit(&Event::Phase {
            phase: format!("{phase:?}"),
        });
    }

    fn on_progress(&self, bytes_done: u64, bytes_total: u64) {
        emit(&Event::Progress {
            bytes_done,
            bytes_total,
        });
    }
}

/// Every line this helper writes to the parent is one JSON [`Event`] --
/// nothing else is ever printed to stdout, so the parent can parse it without
/// having to distinguish protocol output from incidental logging.
fn emit(event: &Event) {
    if let Ok(line) = serde_json::to_string(event) {
        println!("{line}");
        let _ = std::io::stdout().flush();
    }
}
