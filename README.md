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

Argos delivers phase 3 (Windows installer media) validated on real hardware
from both hosts and on both firmwares — see the
[latest release](https://github.com/jp-guimaraes/argos/releases/latest) and
[`CHANGELOG.md`](CHANGELOG.md) for what shipped.

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

### Homebrew (macOS)

```sh
brew install jp-guimaraes/argos/argos
```

Builds from source (there is no pre-built bottle yet); a Rust toolchain is
pulled in automatically as a build dependency. See
[`jp-guimaraes/homebrew-argos`](https://github.com/jp-guimaraes/homebrew-argos).

### apt / `.deb` (Debian, Ubuntu)

Download `argos_<version>_amd64.deb` from the
[latest release](https://github.com/jp-guimaraes/argos/releases/latest),
then:

```sh
sudo apt install ./argos_<version>_amd64.deb
```

A single runtime dependency (`libc6`) -- no `mkfs.ntfs`, no `ntfs-3g`,
nothing else. See `packaging/README.md` for how the package is built.

### Arch / pacman

Not on the AUR yet -- `packaging/aur/PKGBUILD` is in this repository and
validated in CI, but publishing it needs the maintainer's own AUR account
(see `packaging/README.md`). Until then, build it locally:

```sh
git clone https://github.com/jp-guimaraes/argos.git
cd argos/packaging/aur
makepkg -si
```

### Pre-built binaries

For any other platform: Linux (`x86_64-unknown-linux-gnu`) and macOS
(`aarch64-apple-darwin`, `x86_64-apple-darwin`) tarballs are attached to each
[GitHub Release](https://github.com/jp-guimaraes/argos/releases) -- download
the tarball for your platform, extract it, and put `argos` and
`argos-helper` somewhere on your `PATH` (both binaries must stay in the same
directory; `argos` looks for `argos-helper` next to itself first). Neither
binary is code-signed yet, so macOS Gatekeeper will refuse to run `argos` on
first launch until you approve it once in System Settings -> Privacy &
Security.

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

## Usage

```sh
argos list
```

Lists every disk Argos can see and whether it looks safe to write to (not a
system disk, removable, on a USB bus). Start here -- the device path you
need for the next step comes from this output (e.g. `/dev/sdb` on Linux,
`/dev/diskN` on macOS).

```sh
argos write path/to/some.iso --device /dev/sdb
# or, equivalently:
argos write --iso path/to/some.iso --device /dev/sdb
```

`argos` re-elevates itself (`pkexec` on Linux where available, `sudo`
otherwise) to run `argos-helper`, which does the actual write -- expect a
password prompt. Before anything is touched, it prints exactly what it's
about to do and asks you to type the device path back to confirm:

```
About to overwrite:
  device:  /dev/sdb (...)
  size:    ...
  image:   path/to/some.iso

This will PERMANENTLY ERASE all data on /dev/sdb.
Type the device path (/dev/sdb) to confirm:
```

A Linux or Windows installer ISO is detected automatically. Windows media
defaults to `--layout fat32` (GPT/UEFI); add `--layout fat32-bios` for
legacy BIOS/MBR machines instead. Linux ISOs are always written
byte-for-byte ("DD mode") and `--layout` is ignored for them.

```sh
argos verify /dev/sdb --iso path/to/some.iso
```

Re-checks a device against the image it was supposedly written from,
without writing anything -- useful after the fact, or if `write` was run
with `--no-verify`. Unlike `write`, the device here is always positional
and the ISO is always `--iso` -- `write` accepts the ISO either way
precisely so a habit formed on one command doesn't break on the other.

Run `argos <command> --help` for every flag (`--no-verify`, `--no-eject`,
`--i-know-what-im-doing` for a disk Argos doesn't recognize as removable
but you're sure about), or see the man page (`argos man`, or installed
automatically by the `.deb`/Homebrew/AUR packages above) and
`argos completions <shell>` for tab completion.

## Inspiration

At this point, it is clear that Rufus is the primary inspiration for this software. Argos aims to bring that same rock-solid reliability to a cross-platform tool that isn't limited to Windows.
After using Rufus over the years, I always thought "Rufus" sounded like a great dog name. Naturally, this project needed another great dog name to honor the tradition of pairing dependable bootable USB tools with canine names, so Argos it is! 
The backronym came afterward, and it is deliberately not in English: 
**A**ssistente de **R**ebolar **G**ueri-gueris em **O**utros **S**istemas

This is Brazilian Portuguese—specifically, regional slang from the Brazilian Northeast. Two words won't survive a standard dictionary lookup: rebolar here does not mean "to shake hips/dance," but to fling or to toss; and gueri-gueri translates to doodad or knick-knack—a small thing nobody bothers to name properly. Roughly translated, it means "Assistant for Flinging Doodads onto Other Systems", which feels like an honest description of writing an ISO to a flash drive.

Argos was also planned as an experiment to test the current state of AI-assisted software development using agent harnesses. Rust was deliberately chosen because I had zero prior experience with it. On top of that, it involved low-level tasks requiring interaction with hardware, disk partitions, and system BIOS/UEFI. Watching the agents navigate these problems was both fun and eye-opening. A lot of tokens were burned, but a genuinely useful tool was built in less than a week.
