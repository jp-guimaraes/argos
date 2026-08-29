//! `argos-helper`: the one binary in this project meant to run as root.
//!
//! **Not implemented yet** -- this is a placeholder so the workspace and the
//! crate boundary exist from the start (E7 in the backlog is explicit that this
//! separation should never be an afterthought). When implemented, this binary
//! must stay "dumb" by design:
//!   1. Read a single serialized, already-user-confirmed write plan from stdin
//!      (device path, expected serial, expected size, source ISO hash).
//!   2. Re-resolve the device by serial + size *again* right here (the plan may
//!      be stale by the time this process actually runs -- TOCTOU) and abort if
//!      it no longer matches.
//!   3. Open the device exclusively, copy bytes, read them back, report progress
//!      as JSON lines on stdout.
//!   4. Exit. Never linger, never accept a second command, never parse an ISO.
//!
//! CI should enforce that this crate's dependency tree stays tiny (no ISO
//! parsing, no plist/D-Bus/UDisks2 crates) precisely because it runs as root.

fn main() {
    eprintln!("argos-helper: not implemented yet (backlog epic E7)");
    std::process::exit(argos_core::error::ArgosError::NotImplemented("argos-helper").exit_code());
}
