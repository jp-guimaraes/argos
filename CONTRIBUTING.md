# Contributing to Argos

Argos is early-stage; the current state of the project is tracked as a backlog
of epics and stories in [`docs/architecture.md`](docs/architecture.md), which
also documents the design decisions behind the current crate layout.

## Getting set up

- Install a stable Rust toolchain (1.75+). Via [`mise`](https://mise.jdx.dev):
  `mise use -g rust@latest`. Via [`rustup`](https://rustup.rs) works too.
- `cargo build --workspace`
- `cargo test --workspace`
- Before sending a change: `cargo fmt --all` and
  `cargo clippy --workspace --all-targets -- -D warnings` (this is exactly what
  CI runs).

## Workspace layout

- `crates/argos-core` -- all platform-agnostic logic (device model, ISO
  classification, checksums, the write engine). No direct disk/OS I/O; must
  stay testable with plain files and in-memory buffers.
- `crates/argos-platform` -- the `PlatformOps` trait every OS backend
  implements.
- `crates/argos-platform-linux`, `crates/argos-platform-macos`,
  `crates/argos-platform-windows` -- OS-specific implementations of that trait.
  The Windows one is a deliberate stub (out of v1 scope) that exists only to
  keep the trait honestly cross-platform.
- `crates/argos-privileged` -- `argos-helper`, the one binary meant to run as
  root. Keep it minimal and dumb by design (see the doc comment in its
  `main.rs`) -- do not add ISO parsing, D-Bus, or other heavy dependencies to
  this crate.
- `crates/argos-cli` -- the `argos` command-line tool.

## Safety-critical code

Anything touching device selection or the write path exists to answer one
question: *is it safe to write to this disk?* Changes to
`argos_core::device::Device::is_safe_to_write`, the platform crates'
enumeration logic, or the privileged helper's re-validation should:

- come with tests (unit tests for pure logic, the negative "never write a
  system disk" suite for anything device-selection related), and
- err on the side of refusing a device rather than guessing it's safe.

## Commit messages

Commit messages are written in English, regardless of the language used in
discussion or planning.

## License

By contributing, you agree that your contributions will be licensed under
either the MIT license or the Apache License 2.0, at the user's option, as
described in [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
