//! Shared machinery for talking to the privileged `argos-helper` process:
//! locating the binary, elevating (`pkexec`/`sudo`), feeding it a `Plan` as
//! JSON on stdin, and turning its `Event` stream into an `indicatif`
//! progress bar. Used by both `write` (which sends `Plan::Write`) and
//! `verify` (which sends `Plan::Verify`) -- the only difference between the
//! two, from here, is which terminal event carries the hash they print.

use argos_core::error::{ArgosError, Result};
use argos_privileged::protocol::{Event, Plan};
use indicatif::{ProgressBar, ProgressStyle};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

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

/// Reads one JSON [`Event`] per line from the helper, driving a progress bar,
/// until a terminal event settles the outcome (or the stream simply ends).
fn stream_helper_events<R: std::io::Read>(
    reader: BufReader<R>,
) -> Option<std::result::Result<String, ArgosError>> {
    let bar = ProgressBar::new(0);
    bar.set_style(
        ProgressStyle::with_template("{msg} [{bar:40}] {bytes}/{total_bytes} ({eta})")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=> "),
    );

    let mut outcome = None;
    for line in reader.lines().map_while(std::result::Result::ok) {
        let Ok(event) = serde_json::from_str::<Event>(&line) else {
            continue;
        };
        match event {
            Event::Phase { phase } => bar.set_message(phase),
            Event::Progress {
                bytes_done,
                bytes_total,
            } => {
                bar.set_length(bytes_total);
                bar.set_position(bytes_done);
            }
            Event::Done { written_hash } => {
                bar.finish_with_message("done");
                outcome = Some(Ok(written_hash));
            }
            Event::VerifyOk { hash } => {
                bar.finish_with_message("verified");
                outcome = Some(Ok(hash));
            }
            Event::Error { message, .. } => {
                bar.abandon();
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
