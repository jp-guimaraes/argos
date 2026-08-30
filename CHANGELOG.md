# Changelog

All notable changes to Argos are documented here. Loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [1.0.0] - 2026-08-30

Initial release. Argos creates bootable Linux installer USB drives on
Linux and macOS hosts, in the spirit of
[Rufus](https://github.com/pbatard/rufus). See
[`docs/architecture.md`](docs/architecture.md) for the full design.

### Added

- **`argos list`** — lists every disk the platform backend can see, and
  whether each looks safe to write to.
- **`argos write <iso> --device <id>`** — the core destructive flow:
  refreshes the target device, applies the non-negotiable safety gate (a
  system disk is refused unconditionally; a non-removable disk needs
  `--i-know-what-im-doing`), classifies the ISO (refusing anything that
  isn't a hybrid image), runs capacity and source/target-collision
  preflight checks, requires the user to retype the exact device path to
  confirm, unmounts the device, writes it byte-for-byte ("DD mode"),
  verifies the write by reading it back and comparing hashes (`--no-verify`
  to skip), ejects the device afterward (`--no-eject` to skip), and
  reports progress as a live bar — or periodic plain-text lines when
  stdout isn't a real terminal (a pipe, a log file, CI).
- **`argos verify <device> --iso <iso>`** — re-runs post-write
  verification against a device without writing again.
- **Safety model**: a disk is only ever offered for writing when three
  independent signals agree — not a detected system disk, OS-reported
  removable, and on the USB bus. No single OS-reported flag is ever
  trusted alone, and the privileged helper re-checks identity immediately
  before opening the device regardless of what was confirmed earlier (the
  TOCTOU guard).
- **Linux backend**: disk enumeration via `/sys/block/*` and the udev
  database, cross-checked against UDisks2 over D-Bus when reachable.
  System-disk detection resolves LVM/RAID/dm-crypt device-mapper stacks to
  the physical disk underneath.
- **macOS backend**: disk enumeration via `diskutil list`/`info -plist`,
  parsed defensively against schema drift across macOS versions.
  System-disk detection walks through APFS containers (the Apple Silicon
  case, where the boot volume's immediate parent is a virtual container,
  not the real internal disk) to the physical disk underneath.
- **Privilege separation**: `argos-helper`, a small binary deliberately
  kept "dumb", is the only thing that ever opens a raw device, and runs as
  root only for that. It parses no ISO and talks to no D-Bus/plist/UDisks2
  API itself.
- **ISO classification**: distinguishes hybrid ISOs (byte-for-byte
  writable) from El-Torito-only or plain-data images (refused) by
  inspecting the embedded MBR/GPT/El Torito structures already baked into
  the ISO — Argos never creates or modifies a partition table itself, only
  verifies one is already there before trusting a raw copy.
- Pre-built binaries on GitHub Releases for `x86_64-unknown-linux-gnu`,
  `aarch64-apple-darwin`, and `x86_64-apple-darwin`.
- Published on crates.io — install with `cargo install argos-cli
  argos-privileged` (both crates are needed: `argos-cli` alone installs
  `argos` but not the separate `argos-helper` binary it looks for next to
  itself at runtime).

### Verified against real hardware

- Real, official Linux distro ISOs (Ubuntu 26.04.1, 22.04.5, and 18.04.5;
  Alpine Linux 3.24.1) written to real physical USB drives on both Linux
  and macOS hosts, checksum-verified against each distro's own published
  sums before writing and independently re-hashed outside Argos
  afterward.
- Real boot confirmation on both **UEFI** and **BIOS/legacy (MBR)**
  hardware, including one genuinely old BIOS-only machine.
- A write to a real system disk is refused before any privilege
  escalation, with and without `--i-know-what-im-doing`.
- Real-hardware integration tests for the write/verify path without
  needing physical USB media for every run: Linux loop devices, macOS
  `hdiutil`-attached disk images.

### Known limitations

- Windows image support and Windows as a host OS are both out of scope
  for v1 — a future phase.
- No GUI yet; the CLI is architected so one can sit on top of the same
  `argos-core`/`argos-platform` crates later without a rewrite.
- Non-hybrid ISOs are refused rather than partially supported.
- A Homebrew tap is not published yet (crates.io and GitHub Releases are
  both available today).
