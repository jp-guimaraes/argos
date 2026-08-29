//! `argos-helper`: the one binary in this project meant to run as root.
//!
//! Deliberately "dumb" by design (backlog E7): reads a single, already
//! user-confirmed `protocol::WritePlan` from stdin, re-validates the target
//! device against its *current* state (never trusts the plan blindly --
//! `protocol::validate_refreshed_device` is the TOCTOU guard), writes the
//! image, optionally verifies it, and exits. It parses no ISO, talks to no
//! D-Bus/plist/UDisks2 API, and never lingers waiting for a second command.
//! The actual logic lives in `lib.rs::execute` -- this file is only stdin/
//! stdout JSON framing around it.
//!
//! **Known gap**: cancellation is not wired end-to-end yet -- nothing outside
//! this process can currently trigger the `CancelToken` passed to the write
//! loop. A future iteration should forward e.g. a caught SIGINT from the
//! unprivileged parent into a cancel signal here.

use argos_core::progress::{Phase, ProgressSink};
use argos_privileged::protocol::{Event, WritePlan};
use std::io::{Read, Write};

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let plan = match read_plan_from_stdin() {
        Ok(plan) => plan,
        Err(err) => {
            emit(&Event::Error {
                message: format!("failed to read write plan: {err}"),
                exit_code: 1,
            });
            return 1;
        }
    };

    match argos_privileged::execute(&plan, &JsonlProgress) {
        Ok(written_hash) => {
            emit(&Event::Done { written_hash });
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

fn read_plan_from_stdin() -> std::io::Result<WritePlan> {
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
