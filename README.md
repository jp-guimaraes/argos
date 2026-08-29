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

Argos is under active, early development. The current scope (v1) targets:

- **Images**: Linux ISOs only (including isohybrid images), written
  byte-for-byte in "DD mode". Windows image support is a planned future phase.
- **Hosts**: Linux (implemented) and macOS (planned). Windows-as-host is out
  of scope for now.
- **Interface**: a CLI (`argos`), architected so a GUI can be added later
  without reworking the core logic.

See [`docs/architecture.md`](docs/architecture.md) for the full design and a
per-area status table, and [`CONTRIBUTING.md`](CONTRIBUTING.md) for how to
build and test the project.

## Inspiration

A good source of inspiration is
[Rufus](https://github.com/pbatard/rufus), a free-software tool for Windows.
Argos aims to bring the same reliability to a cross-platform tool that isn't
limited to Windows.
