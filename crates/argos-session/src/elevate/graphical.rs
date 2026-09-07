//! Elevation for a front end with no controlling terminal, on macOS.
//!
//! `sudo` cannot run here at all: with no terminal it has nowhere to read a
//! password from and fails with "a terminal is required". What replaces it is
//! `osascript`'s `do shell script ... with administrator privileges`, which
//! puts up the system's own authorization dialog -- so **Argos never sees the
//! password**, and none of this needs code signing or a Developer ID.
//!
//! The catch is that `do shell script` hands back stdout only when the
//! command has finished, which would cost both the progress bar and
//! cancellation. A pair of FIFOs stands in for the pipes `Command::spawn`
//! would have given us: `sh` runs as root with `< plan > events`, so the
//! helper's stdin and stdout are those FIFOs and **nothing in
//! `argos-privileged` changes** -- the cancel byte, the EOF-means-cancel
//! safety net and the JSONL event stream all work exactly as they do over a
//! pipe.
//!
//! Both FIFOs are opened `O_RDWR`, which never blocks. That is load-bearing
//! rather than a micro-optimization: opening each end blocking looks like a
//! clean handshake with `sh`'s two redirections, and is one -- right up until
//! the user dismisses the authorization dialog. Then `osascript` exits, `sh`
//! never runs, no counterpart end is ever opened, and both opens block
//! forever. Confirmed on macOS 26.6.2 with the parent still alive and stuck
//! after the child was already gone. `O_RDWR` also keeps the
//! EOF-on-parent-death semantics `protocol::watch_for_cancel` relies on:
//! what makes the helper's `read()` return 0 is the last *write* end closing,
//! and our handle is one of those.

use super::{EventStream, FifoEvents, RunDir, Running};
use crate::cancel::Canceller;
use argos_core::error::{ArgosError, Result};
use argos_privileged::protocol::Plan;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;

pub(super) fn spawn(plan: &Plan, helper_path: &Path) -> Result<Running> {
    let rundir = make_run_dir()?;
    let plan_fifo = rundir.0.join("plan");
    let events_fifo = rundir.0.join("events");
    mkfifo(&plan_fifo)?;
    mkfifo(&events_fifo)?;

    // Opened before the child exists: with O_RDWR neither call blocks, so
    // there is no ordering constraint left to get wrong, and `sh`'s two
    // redirections find both ends already present.
    let mut plan_channel = open_rdwr(&plan_fifo)?;
    let events_keepalive = open_rdwr(&events_fifo)?;

    let plan_json = serde_json::to_string(plan).map_err(std::io::Error::other)?;
    writeln!(plan_channel, "{plan_json}")?;
    plan_channel.flush()?;

    // Read on a thread with a plain blocking O_RDONLY open, which returns
    // immediately because `events_keepalive` above is already a writer.
    let (tx, rx) = mpsc::channel();
    let events_path = events_fifo.clone();
    let reader = std::thread::spawn(move || {
        let Ok(file) = File::open(&events_path) else {
            return;
        };
        for line in BufReader::new(file)
            .lines()
            .map_while(std::result::Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // 0600 like the FIFOs beside it. The script holds no secret -- the same
    // paths are visible in `ps` -- but it is the exact text that will run as
    // root, and inside a 0700 directory there is no reason for it to be
    // world-readable as well.
    let script_path = rundir.0.join("run.scpt");
    write_private(
        &script_path,
        applescript(helper_path, &plan_fifo, &events_fifo)?.as_bytes(),
    )?;

    let child = Command::new("/usr/bin/osascript")
        .arg(&script_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Captured, not inherited: this is where AppleScript reports a
        // dismissed dialog as "User canceled. (-128)", which
        // `Running::no_result_error` turns into `NotConfirmed`.
        .stderr(Stdio::piped())
        .spawn()?;

    let mut child = child;
    let stderr = child.stderr.take();
    Ok(Running {
        events: EventStream::Fifo(FifoEvents {
            lines: rx,
            keepalive: Some(events_keepalive),
            reader: Some(reader),
            child_gone_since: None,
        }),
        // The held-open plan FIFO *is* the ChildStdin equivalent: the same
        // cancel byte, the same EOF on close.
        canceller: Canceller::new(Box::new(plan_channel)),
        child,
        stderr,
        _rundir: Some(rundir),
    })
}

/// The script is written to a file rather than passed with `-e` so quoting
/// can only go wrong in one place, here, where it is checked.
fn applescript(helper: &Path, plan_fifo: &Path, events_fifo: &Path) -> Result<String> {
    let helper = shell_quote(helper)?;
    let plan_fifo = shell_quote(plan_fifo)?;
    let events_fifo = shell_quote(events_fifo)?;
    Ok(format!(
        // `with timeout` because a multi-gigabyte write takes far longer than
        // AppleScript's default patience, and being killed halfway is exactly
        // what must not happen to a device being partitioned.
        "with timeout of 86400 seconds\n\
         do shell script \"{helper} < {plan_fifo} > {events_fifo}\" \
         with prompt \"Argos needs administrator access to write to the selected disk.\" \
         with administrator privileges\n\
         end timeout\n"
    ))
}

/// Single-quotes a path for `/bin/sh`, refusing anything that would need
/// escaping inside the AppleScript string literal that wraps it.
///
/// The FIFO paths come from [`make_run_dir`] and contain no quotes by
/// construction; the helper path does not either, in any layout Argos ships.
/// Refusing is still the right answer to a path that would: a mis-quoted
/// string here becomes a shell command running as root.
fn shell_quote(path: &Path) -> Result<String> {
    let text = path.to_str().ok_or_else(|| {
        ArgosError::Io(std::io::Error::other(format!(
            "{} is not valid UTF-8 and cannot be elevated safely",
            path.display()
        )))
    })?;
    if text.contains('\'') || text.contains('"') || text.contains('\\') {
        return Err(ArgosError::Io(std::io::Error::other(format!(
            "{text} contains a quote or backslash; refusing to build a privileged shell \
             command around it"
        ))));
    }
    Ok(format!("'{text}'"))
}

/// A private, per-run directory. 0700 because the `Plan` names device paths,
/// and there is no reason for another user on the machine to read them --
/// though the plan itself never touches disk, only the FIFO.
fn make_run_dir() -> Result<RunDir> {
    let base = std::env::temp_dir();
    for attempt in 0..16 {
        let candidate = base.join(format!(
            "argos-{}-{}-{attempt}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        match std::fs::DirBuilder::new().mode(0o700).create(&candidate) {
            Ok(()) => return Ok(RunDir(candidate)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(ArgosError::Io(err)),
        }
    }
    Err(ArgosError::Io(std::io::Error::other(
        "could not create a private directory for the elevation channel",
    )))
}

fn mkfifo(path: &Path) -> Result<()> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("a NUL byte in the elevation channel path"))?;
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the
    // call, and mkfifo(2) touches nothing else.
    if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
        return Err(ArgosError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

/// See this module's doc comment: `O_RDWR` on a FIFO never blocks, which is
/// what stops a dismissed authorization dialog from hanging the caller.
fn open_rdwr(path: &Path) -> Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(ArgosError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_dir_is_private_and_removes_itself() {
        use std::os::unix::fs::PermissionsExt;
        let path = {
            let dir = make_run_dir().expect("create");
            let mode = std::fs::metadata(&dir.0).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "the plan names device paths");
            dir.0.clone()
        };
        assert!(!path.exists(), "the run directory outlived its RunDir");
    }

    #[test]
    fn a_fifo_is_created_and_is_a_fifo() {
        use std::os::unix::fs::FileTypeExt;
        let dir = make_run_dir().expect("create");
        let fifo = dir.0.join("plan");
        mkfifo(&fifo).expect("mkfifo");
        assert!(std::fs::metadata(&fifo).unwrap().file_type().is_fifo());
    }

    /// The property the whole scheme rests on: neither open blocks, so a
    /// dismissed dialog cannot hang the caller. If this ever regresses, the
    /// test hangs rather than fails -- which is itself the signal.
    #[test]
    fn opening_both_ends_never_blocks() {
        let dir = make_run_dir().expect("create");
        let fifo = dir.0.join("events");
        mkfifo(&fifo).expect("mkfifo");
        let a = open_rdwr(&fifo).expect("first open");
        let b = open_rdwr(&fifo).expect("second open");
        drop((a, b));
    }

    /// Everything the elevation channel puts on disk stays private to the
    /// user: the directory, both FIFOs, and the script that will run as root.
    #[test]
    fn the_script_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = make_run_dir().expect("create");
        let script = dir.0.join("run.scpt");
        write_private(&script, b"noop").expect("write");
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn the_script_quotes_every_path_it_interpolates() {
        let script = applescript(
            Path::new("/usr/local/bin/argos-helper"),
            Path::new("/tmp/argos-1/plan"),
            Path::new("/tmp/argos-1/events"),
        )
        .expect("plain paths must build");
        assert!(script.contains(
            "'/usr/local/bin/argos-helper' < '/tmp/argos-1/plan' > '/tmp/argos-1/events'"
        ));
        assert!(script.contains("with administrator privileges"));
        assert!(script.contains("with timeout of 86400 seconds"));
    }

    /// A mis-quoted path here becomes a shell command running as root, so
    /// the answer is to refuse rather than to escape cleverly.
    #[test]
    fn a_path_with_a_quote_is_refused_rather_than_escaped() {
        for hostile in [
            "/tmp/it's/argos-helper",
            "/tmp/\"quoted\"/argos-helper",
            "/tmp/back\\slash/argos-helper",
        ] {
            assert!(
                shell_quote(Path::new(hostile)).is_err(),
                "{hostile} should be refused"
            );
        }
    }
}
