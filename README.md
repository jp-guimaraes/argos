# Argos

Argos is a free-software tool for creating bootable installer USB drives, in
the spirit of [Rufus](https://github.com/pbatard/rufus) but built to run
cross-platform.

## The name

**Argos** was picked to echo *Rufus*: both are dog names. Argos is also
Odysseus's dog in the *Odyssey* — the one who waits twenty years and is the
only one who recognizes him when he finally comes home, which is not a bad
thing to name a tool you want people to trust with their only USB stick.

The backronym came afterwards, and it is deliberately **not** in English:

> **A**ssistente de **R**ebolar **G**ueri-gueris em **O**utros **S**istemas

That is Brazilian Portuguese, in the Northeastern register the author speaks.
Two words do not survive a dictionary: *rebolar* here does not mean "to sway"
but **to fling** or **to toss**, and a *gueri-gueri* is a **doodad** — a small
thing nobody bothers to name properly. Roughly, *"Assistant for Flinging
Doodads onto Other Systems"*, which is an honest description of writing an ISO
to a USB drive.

*Outros sistemas* carries the joke: Rufus runs on Windows, and Argos is for
the **other** systems — both the ones it runs on and the ones it writes for.

## Goal

Argos should be able to create installer USB drives for both Windows and
Linux distributions, targeting both legacy MBR/BIOS and GPT/UEFI machines.

## Requirements

The software should be reliable and fast, and should be installable across a
wide range of Linux distributions and, ideally, on macOS as well.

## Status

Argos is at **v1.5.0**, which delivers phase 3 (Windows installer media)
validated on real hardware from both hosts and on both firmwares — see
[`CHANGELOG.md`](CHANGELOG.md).

- **Images**: Linux ISOs (including isohybrid images), written byte-for-byte
  in "DD mode"; and Windows 10/11 installer ISOs, written as a FAT32 volume
  with `install.wim` split into `install.swm` parts where it exceeds FAT32's
  4 GiB file limit.
- **Targets**: UEFI firmware (`--layout fat32`) and legacy BIOS machines
  (`--layout fat32-bios`, which carries Argos's own MBR and FAT32 boot
  records). Both confirmed to reach Windows Setup's disk selection on real
  machines.
- **Hosts**: Linux and macOS, both implemented — including for Windows media,
  which needs no `mkfs`, no FUSE and no Windows machine anywhere in the
  process. Windows-as-host is out of scope for now.
- **Interface**: a CLI (`argos`), architected so a GUI can be added later
  without reworking the core logic. No GUI exists today.

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

> **macOS with Homebrew's `rustup`**: `cargo install` puts its binaries in
> `~/.cargo/bin`, and Homebrew's keg-only `rustup` formula does **not** add
> that directory to your `PATH` — the official `rustup-init` installer does,
> which is why this bites Homebrew users specifically. If `argos` comes back
> as `command not found` immediately after a successful install, that is all
> this is:
>
> ```sh
> export PATH="$HOME/.cargo/bin:$PATH"
> ```
>
> Add it to your `~/.zshrc` to make it stick.

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
