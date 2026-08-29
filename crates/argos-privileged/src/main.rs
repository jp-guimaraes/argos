//! `argos-helper`: the one binary in this project meant to run as root.
//!
//! Deliberately "dumb" by design (backlog E7): reads a single, already
//! user-confirmed [`protocol::WritePlan`] from stdin, re-validates the target
//! device against its *current* state (never trusts the plan blindly --
//! [`protocol::validate_refreshed_device`] is the TOCTOU guard), writes the
//! image, optionally verifies it, and exits. It parses no ISO, talks to no
//! D-Bus/plist/UDisks2 API, and never lingers waiting for a second command.
//!
//! **Known gap**: cancellation is not wired end-to-end yet -- nothing outside
//! this process can currently trigger the `CancelToken` passed to the write
//! loop. A future iteration should forward e.g. a caught SIGINT from the
//! unprivileged parent into a cancel signal here.

mod platform_select;

use argos_core::error::ArgosError;
use argos_core::progress::{CancelToken, Phase, ProgressSink};
use argos_core::write::dd_mode;
use argos_core::{preflight, verify};
use argos_platform::PlatformOps;
use argos_privileged::protocol::{self, Event, WritePlan};
use std::fs::{File, OpenOptions};
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

    match execute(&plan) {
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

fn execute(plan: &WritePlan) -> Result<String, ArgosError> {
    let platform = platform_select::current_platform();

    let refreshed = platform.refresh(&plan.device_path, plan.expected_serial.as_deref())?;
    protocol::validate_refreshed_device(plan, refreshed.as_ref())?;

    let mut image = File::open(&plan.image_path)?;
    let mut device = OpenOptions::new().write(true).open(&plan.device_path)?;

    preflight::check_capacity(
        &plan.device_path,
        plan.expected_size_bytes,
        &plan.image_path,
        plan.image_size_bytes,
    )?;

    let progress = JsonlProgress;
    // No cancellation source is wired up yet -- see the module doc comment.
    let cancel = CancelToken::new();

    let written_hash = dd_mode::write_stream(
        &mut image,
        &mut device,
        plan.image_size_bytes,
        &progress,
        &cancel,
    )?;
    device.flush()?;
    drop(device);

    if plan.verify {
        let mut device_for_read = File::open(&plan.device_path)?;
        verify::verify_written_image(
            &mut device_for_read,
            plan.image_size_bytes,
            &written_hash,
            &progress,
        )?;
    }

    Ok(written_hash)
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
