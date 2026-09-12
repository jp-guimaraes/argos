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
from both hosts and on both firmwares, and phase 4 (a graphical interface)
sharing the CLI's exact safety guarantees -- see the
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
- **Interface**: a CLI (`argos`) and a single-window GUI (`argos-gui`,
  currently macOS and Linux), sharing one implementation of every safety
  check -- there is no separate, weaker path through the window. Shipped
  since 1.6.0; see [Installation](#installation) below for where each one
  is available, and [Graphical interface](#graphical-interface) for a
  walkthrough.

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

### GUI (macOS, `.dmg`)

Each release also attaches a universal `Argos-<version>.dmg` (Apple Silicon
and Intel in one file) -- open it, drag `Argos.app` into `Applications`.

It is unsigned and unnotarized, the same decision as the CLI binaries above
and for the same reason: this project's macOS install story is Homebrew,
which never sets the quarantine bit that triggers Gatekeeper in the first
place, so the `.dmg` is a convenience download rather than the primary path.
A downloaded, quarantined `Argos.app` will be refused on first open. Clear
the quarantine attribute once, from a terminal:

```sh
xattr -dr com.apple.quarantine /Applications/Argos.app
```

or, from Finder: System Settings -> Privacy & Security -> scroll to the
bottom, where an "Open Anyway" button appears after the first blocked
attempt.

Once running, `Argos.app` needs **Full Disk Access** (System Settings ->
Privacy & Security -> Full Disk Access) to write to a removable drive -- the
same permission macOS requires of Disk Utility and similar tools. Without
it, a write fails right after unmounting with a plain `Operation not
permitted`. It is specifically `argos-helper` (inside the bundle, at
`Argos.app/Contents/MacOS/argos-helper`) that needs the grant, not
`Argos.app` itself -- System Settings' own "+" file picker cannot navigate
into a `.app` bundle, so add it by dragging that file from a Finder window
(right-click `Argos.app` -> "Show Package Contents" to reach it) onto the
Full Disk Access list instead.

**This grant does not survive an update.** Each release rebuilds
`argos-helper` with a new ad-hoc code signature, which macOS treats as a
different program -- upgrading to a new version of Argos means re-adding it
to Full Disk Access again, not just on first install. See
`packaging/README.md` for the full story and the log evidence.

Not yet available as a Homebrew cask -- see `packaging/README.md`.

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

### Command line

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

### Graphical interface

`argos-gui` is a single window over the exact same safety checks as the CLI
above -- there is no separate, weaker path through it. See
[Installation](#installation) for where to get it (macOS `.dmg`/Homebrew,
Linux `.deb`/AUR); it ships alongside `argos` and `argos-helper` in every
package.

<img src="https://raw.githubusercontent.com/jp-guimaraes/argos/main/docs/images/gui/first-launch-pt-br.png" alt="Argos GUI on first launch, automatically in Brazilian Portuguese" width="360">

On first launch it detects the system language automatically (`LANG`/
`LC_ALL` on Linux, `AppleLanguages` on macOS) -- no setup needed for a
`pt_BR` lab machine, as pictured above. Automatic detection, English and
Português (Brasil) are also selectable by hand from the menu at the top
right, and take effect immediately, no restart:

<img src="https://raw.githubusercontent.com/jp-guimaraes/argos/main/docs/images/gui/language-switch.png" alt="The GUI's language menu: Automatic, English, Português (Brasil)" width="360">

**Image**: click "Choose..." or drag an ISO onto the window. Argos identifies
it as a Linux image or a Windows installer the same way `argos write` does,
and shows which:

<img src="https://raw.githubusercontent.com/jp-guimaraes/argos/main/docs/images/gui/mbr-option-en.png" alt="A Windows ISO selected, with the legacy-BIOS/MBR checkbox enabled and explained" width="360">

For a Windows installer, an "Old machine -- legacy BIOS (MBR)" checkbox
appears (disabled, with an explanatory line, until a Windows image is
chosen) -- this is `--layout fat32-bios` from the CLI, for pre-UEFI
machines; left unchecked, media is written for UEFI (`--layout fat32`).
Linux images have no such choice: they are always written byte-for-byte.

**Target**: the dropdown lists only disks Argos considers safe to write to
(removable, on a USB bus, not the system disk) -- a device it refuses is a
device you cannot click by accident. "Show every disk, including those the
system does not consider removable" reveals the rest for inspection, but
existing refusals (system disk, no removable bus) still apply; it doesn't
unlock anything, only widens what's listed. With nothing plugged in, the
dropdown says so plainly rather than showing an empty list:

<img src="https://raw.githubusercontent.com/jp-guimaraes/argos/main/docs/images/gui/no-device.png" alt="The target dropdown reporting no removable device found" width="360">

**Write / Verify**: both stay disabled until an image and a target are
picked. "Write..." opens the same confirmation as the CLI's "type the
device path back" prompt -- retyping it is not a checkbox to click through,
on the window either. Elevation happens from there (a password dialog on
macOS, a polkit prompt on Linux); once running, progress, cancel, and any
error are reported in the window itself, translated into whichever language
is active. "Verify..." re-checks a device against an image without writing
anything, same as `argos verify`.

## Inspiration

At this point, it is clear that Rufus is the primary inspiration for this software. Argos aims to bring that same rock-solid reliability to a cross-platform tool that isn't limited to Windows.
After using Rufus over the years, I always thought "Rufus" sounded like a great dog name. Naturally, this project needed another great dog name to honor the tradition of pairing dependable bootable USB tools with canine names, so Argos it is! 
The backronym came afterward, and it is deliberately not in English: 
**A**ssistente de **R**ebolar **G**ueri-gueris em **O**utros **S**istemas

This is Brazilian Portuguese—specifically, regional slang from the Brazilian Northeast. Two words won't survive a standard dictionary lookup: rebolar here does not mean "to shake hips/dance," but to fling or to toss; and gueri-gueri translates to doodad or knick-knack—a small thing nobody bothers to name properly. Roughly translated, it means "Assistant for Flinging Doodads onto Other Systems", which feels like an honest description of writing an ISO to a flash drive.

Argos was also planned as an experiment to test the current state of AI-assisted software development using agent harnesses. Rust was deliberately chosen because I had zero prior experience with it. On top of that, it involved low-level tasks requiring interaction with hardware, disk partitions, and system BIOS/UEFI. Watching the agents navigate these problems was both fun and eye-opening. A lot of tokens were burned, but a genuinely useful tool was built in less than a week.
