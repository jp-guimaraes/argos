# Argos

Argos is a free-software tool for creating bootable installer USB drives, in
the spirit of [Rufus](https://github.com/pbatard/rufus) but built to run
cross-platform.

## Goal

Argos should be able to create installer USB drives for both Windows and
Linux distributions, targeting both legacy MBR/BIOS and GPT/UEFI machines.

## Requirements

The software should be reliable and fast, and should be installable across a
wide range of Linux distributions and, ideally, on macOS as well.

## Status

Argos has reached v1.0. The v1 scope:

- **Images**: Linux ISOs only (including isohybrid images), written
  byte-for-byte in "DD mode". Windows image support is a planned future phase.
- **Hosts**: Linux and macOS, both implemented. Windows-as-host is out of
  scope for now.
- **Interface**: a CLI (`argos`), architected so a GUI can be added later
  without reworking the core logic.

See [`docs/architecture.md`](docs/architecture.md) for the full design and a
per-area status table, [`CHANGELOG.md`](CHANGELOG.md) for what shipped in
each release, and [`CONTRIBUTING.md`](CONTRIBUTING.md) for how to build and
test the project.

## Installation

Pre-built binaries for Linux (`x86_64-unknown-linux-gnu`) and macOS
(`aarch64-apple-darwin`, `x86_64-apple-darwin`) are attached to each
[GitHub Release](https://github.com/jp-guimaraes/argos/releases) -- download
the tarball for your platform, extract it, and put `argos` and
`argos-helper` somewhere on your `PATH` (both binaries must stay in the same
directory; `argos` looks for `argos-helper` next to itself first). Neither
binary is code-signed yet, so macOS Gatekeeper will refuse to run `argos` on
first launch until you approve it once in System Settings -> Privacy &
Security.

See [Releases](https://github.com/jp-guimaraes/argos/releases) for the
current tarballs.

### Via `cargo install`

```sh
cargo install argos-cli argos-privileged
```

Installs both `argos` (from the `argos-cli` crate) and `argos-helper` (from
the separate, privilege-separated `argos-privileged` crate -- see
[`docs/architecture.md`](docs/architecture.md)) into `cargo`'s install
directory (`~/.cargo/bin` by default), which is what puts them next to each
other. Passing only `argos-cli` installs `argos` without the helper binary it
needs at runtime -- always install both together.

### From source

```sh
git clone https://github.com/jp-guimaraes/argos.git
cd argos
cargo build --release -p argos-cli -p argos-privileged
# binaries land in target/release/argos and target/release/argos-helper
```

## Inspiration

A good source of inspiration is
[Rufus](https://github.com/pbatard/rufus), a free-software tool for Windows.
Argos aims to bring the same reliability to a cross-platform tool that isn't
limited to Windows.
