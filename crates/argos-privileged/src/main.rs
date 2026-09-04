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

use argos_core::progress::{CancelToken, Phase, ProgressSink};
use argos_privileged::protocol::{self, Event, Plan};
use std::io::{BufRead, Write};

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    ignore_sigint();

    // Deliberately the unlocked `Stdin` handle, not `.lock()`'s `StdinLock`:
    // that one holds a `MutexGuard` for as long as it is alive, which is not
    // `Send` and so cannot be handed to the watcher thread below. `Stdin`
    // itself is `Send + Sync` -- each read takes the lock internally, just for
    // that call -- so `read_plan` borrows it for the first line and the same
    // handle then moves into the thread for the rest of the stream.
    let stdin = std::io::stdin();
    let plan = match read_plan(&mut stdin.lock()) {
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

    // Which device to eject once the operation has succeeded, if the plan
    // asked for one. Recorded here because each arm below consumes its plan,
    // and acted on only after the terminal event -- see `Event::Ejected`.
    let mut eject_after: Option<String> = None;

    let result = match plan {
        Plan::Write(write_plan) => {
            if write_plan.eject {
                eject_after = Some(write_plan.device_path.clone());
            }
            argos_privileged::execute(&write_plan, &JsonlProgress, &cancel)
                .map(|written_hash| Event::Done { written_hash })
        }
        Plan::Verify(verify_plan) => argos_privileged::execute_verify(&verify_plan, &JsonlProgress)
            .map(|hash| Event::VerifyOk { hash }),
        // The plan's layout (WindowsLayout::Fat32 or Fat32Bios -- GPT/UEFI
        // or MBR/BIOS) selects the partition table inside the same FAT32
        // executor; there is only one Windows write path since NTFS's
        // retirement (decision point M4.3, see docs/architecture.md).
        Plan::WriteWindowsImage(windows_plan) => {
            if windows_plan.eject {
                eject_after = Some(windows_plan.device_path.clone());
            }
            argos_privileged::windows_fat32::execute_write_windows_fat32(
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
            argos_privileged::windows_fat32::execute_verify_windows_fat32(
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
            // After the terminal event, deliberately: the write is already
            // done and verified, so a failure here is a warning about
            // unplugging rather than a failed operation, and the CLI's
            // progress bar has finished by the time it prints.
            if let Some(device_path) = eject_after {
                let error = argos_privileged::eject_device(&device_path)
                    .err()
                    .map(|err| err.to_string());
                emit(&Event::Ejected { device_path, error });
            }
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

/// Reads exactly the first line of `stdin` -- the `Plan` -- and leaves the
/// rest of the pipe for [`protocol::watch_for_cancel`]. Reading to EOF here,
/// as this used to, would consume the cancel byte and block until the parent
/// closed the pipe, which is precisely what cancellation needs it not to do.
fn read_plan<R: BufRead>(reader: &mut R) -> std::io::Result<Plan> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    serde_json::from_str(&line).map_err(std::io::Error::other)
}

/// Makes `SIGINT` a no-op for this process.
///
/// Ctrl-C in a terminal goes to the whole foreground process group, and the
/// privilege broker leaves the helper in it -- so without this, the default
/// disposition kills the helper outright, mid-write, *before* the write path
/// can act on a cancellation. Everything cancellation is supposed to do on the
/// way out -- most importantly destroying the half-written FAT32 volume so it
/// cannot be mistaken for good media -- would never run.
///
/// Cancellation therefore has exactly one channel: the byte `argos` writes on
/// stdin (plus the pipe closing, which `watch_for_cancel` treats the same way
/// as a safety net for a parent that died without asking politely). `SIGKILL`
/// still cannot be caught, and a helper killed that way still leaves media
/// that must be rewritten -- which is what `ArgosError::Cancelled` has always
/// said.
fn ignore_sigint() {
    // SAFETY: `SIG_IGN` is a disposition, not a Rust callback -- there is no
    // handler body to be async-signal-unsafe.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use argos_privileged::protocol::{WindowsLayout, WriteWindowsPlan};
    use std::io::Read;

    /// The regression that would silently disable cancellation: reading to
    /// EOF here, as this used to, consumes the cancel byte the watcher thread
    /// is supposed to see -- and blocks until the parent closes the pipe,
    /// which is exactly what it must not wait for.
    #[test]
    fn read_plan_consumes_the_plan_line_and_nothing_after_it() {
        let plan = Plan::WriteWindowsImage(WriteWindowsPlan {
            device_path: "/dev/null".into(),
            expected_serial: None,
            expected_size_bytes: 0,
            iso_path: "/tmp/x.iso".into(),
            layout: WindowsLayout::Fat32,
            eject: false,
        });
        let json = serde_json::to_string(&plan).unwrap();

        let mut stream = std::io::Cursor::new(format!("{json}\n").into_bytes());
        stream.get_mut().push(protocol::CANCEL_SIGNAL);

        read_plan(&mut stream).expect("the plan line should parse");

        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).unwrap();
        assert_eq!(
            rest,
            vec![protocol::CANCEL_SIGNAL],
            "the cancel byte must still be on the stream for the watcher thread"
        );
    }
}
