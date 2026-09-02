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
//! **Known gap**: cancellation is not wired end-to-end yet -- nothing outside
//! this process can currently trigger the `CancelToken` passed to the write
//! loop. A future iteration should forward e.g. a caught SIGINT from the
//! unprivileged parent into a cancel signal here.

use argos_core::progress::{Phase, ProgressSink};
use argos_privileged::protocol::{Event, Plan, WindowsLayout};
use std::io::{Read, Write};

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let plan = match read_plan_from_stdin() {
        Ok(plan) => plan,
        Err(err) => {
            emit(&Event::Error {
                message: format!("failed to read plan: {err}"),
                exit_code: 1,
            });
            return 1;
        }
    };

    let result = match plan {
        Plan::Write(write_plan) => argos_privileged::execute(&write_plan, &JsonlProgress)
            .map(|written_hash| Event::Done { written_hash }),
        Plan::Verify(verify_plan) => argos_privileged::execute_verify(&verify_plan, &JsonlProgress)
            .map(|hash| Event::VerifyOk { hash }),
        Plan::WriteWindowsImage(windows_plan) => match windows_plan.layout {
            WindowsLayout::Ntfs => argos_privileged::windows::execute_write_windows_image(
                &windows_plan,
                &JsonlProgress,
            )
            .map(|outcome| Event::WindowsDone {
                files_copied: outcome.files_copied,
                bytes_copied: outcome.bytes_copied,
            }),
            WindowsLayout::Fat32 => argos_privileged::windows_fat32::execute_write_windows_fat32(
                &windows_plan,
                &JsonlProgress,
            )
            .map(|outcome| Event::WindowsDone {
                files_copied: outcome.files_copied,
                bytes_copied: outcome.bytes_copied,
            }),
        },
        Plan::VerifyWindowsImage(verify_windows_plan) => match verify_windows_plan.layout {
            WindowsLayout::Ntfs => argos_privileged::windows::execute_verify_windows_image(
                &verify_windows_plan,
                &JsonlProgress,
            )
            .map(|outcome| Event::WindowsVerifyOk {
                files_verified: outcome.files_verified,
            }),
            WindowsLayout::Fat32 => argos_privileged::windows_fat32::execute_verify_windows_fat32(
                &verify_windows_plan,
                &JsonlProgress,
            )
            .map(|outcome| Event::WindowsVerifyOk {
                files_verified: outcome.files_verified,
            }),
        },
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

fn read_plan_from_stdin() -> std::io::Result<Plan> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    serde_json::from_str(&input).map_err(std::io::Error::other)
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
