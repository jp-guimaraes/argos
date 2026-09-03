# Changelog

All notable changes to Argos are documented here. Loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

Phase 3: **self-contained Windows installer media**, on Linux *and* macOS,
for **UEFI *and* legacy BIOS** machines. See
[`docs/plan-phase3-self-contained.md`](docs/plan-phase3-self-contained.md)
for the plan this delivers against.

The headline: producing Windows install media no longer requires a Windows
machine, an external `mkfs`, a FUSE filesystem, or a vendored binary blob.

### Added

- **`image::udf`** — Argos's own streaming UDF reader (M1, #40). Official
  Windows ISOs are UDF bridges, and the previous dependency read whole files
  into memory, which OOMed on a 5GB `install.wim`. Copying now runs in
  constant memory: **3 MiB peak RSS** streaming a 7.58GB file, with a digest
  byte-identical to macOS's own UDF driver reading the same file.
- **`image::wim`** — Argos's own WIM reader and splitter (M2, #42). Splits
  `install.wim` into `install.swm` parts under FAT32's 4GiB limit by
  redistributing whole stored resources verbatim: nothing is decompressed or
  re-encoded, so the lookup table's SHA-1s stay valid by construction and no
  XPRESS/LZX codec is needed. Validated against wimlib as an external oracle
  (including `wimlib-imagex apply` reproducing a source tree byte for byte
  from our parts) and, ultimately, by Windows Setup itself on real hardware.
- **`--layout fat32`** — a single pure-Rust FAT32 partition (M3, #43),
  written directly into the partition's byte range of the open device: no
  `mkfs`, no mount, no partition device nodes, no kernel filesystem driver.
  GPT-partitioned; boots UEFI firmware via the ISO's own `bootx64.efi`.
- **`--layout fat32-bios`** — the same media, MBR-partitioned and carrying
  **Argos's own boot records** (M6, #45): an MBR bootstrap in 279 of the 440
  bytes available, and a FAT32 volume boot record in 418 of 420, both written
  from scratch in 16-bit assembly under MIT/Apache rather than porting
  `ms-sys`'s GPL binary blobs. Boots legacy BIOS machines.
- **Windows write support on macOS** (M4, #34) — enabled by the FAT32 route,
  which superseded the original macFUSE/`ntfs-3g` plan entirely.
- **`tools/mediadiff.py`** — dumps a written device's full structure (MBR,
  GPT, BPB, FAT, directory tree) and diffs two dumps, with ~25 conformance
  checks. Written to run a differential diagnosis against Rufus-made media;
  it is what found the stale-GPT bug below.

- **End-to-end write cancellation** (M7.5, #35). Ctrl-C during a write now
  reaches the privileged helper: `argos` keeps the helper's stdin open and a
  `SIGINT` handler writes `protocol::CANCEL_SIGNAL` into it, a watcher thread
  in the helper turns that into a `CancelToken`, and the write path checks it
  on every buffer. The helper **ignores `SIGINT` itself** -- Ctrl-C reaches
  the whole foreground process group, and letting the default disposition kill
  the helper would stop it before it could act on the cancellation. The pipe
  closing counts as a cancellation too, so a parent that dies outright still
  stops the write.

### Fixed

- A stale GPT surviving underneath the MBR layout (#59). `mbrman` writes
  sector 0 and nothing else, so a stick recycled from `--layout fat32` kept
  its entire GPT — primary header, entry array, and backup header in the
  device's last sector, CRCs intact — under a non-protective MBR. Windows
  refuses to assign a drive letter to a volume on a disk in that state,
  while the media still boots, which made it hard to localize.
- Directory entries `fatfs` writes that violate the FAT specification: long
  filename entries placed before `.` and `..`, and `..` pointing at the
  root's cluster instead of 0 (#56, confirmed upstream).
- `BPB_HiddSec` left at 0 on the GPT path, and a BPB CHS geometry (32×64)
  that contradicted the 255×63 the MBR partition entries are built from.
- A fixed volume serial number, so every medium Argos wrote claimed the same
  identity; and a previous bootloader surviving in sector 0 across a GPT
  write.
- Media writes now cost roughly **8× fewer I/O operations**.

### Removed

- `hadris-udf`, and with it the `check_windows_memory` preflight that existed
  only to refuse writes it would have OOMed on.
- **The NTFS/UEFI:NTFS Windows write path** (`--layout ntfs`), retired at
  decision point M4.3 once `--layout fat32`/`fat32-bios` was validated on
  real hardware from both hosts and both firmwares (see below). Out of the
  tree, not just unused: the vendored `uefi-ntfs.img` boot image and its
  provenance doc, the `mkfs.ntfs`/`ntfs-3g` shell-outs, the two-partition
  `WindowsPartitionPlan`, and the three `PlatformOps` methods that existed
  only to support it (`reread_partition_table`, `mount_ntfs_partition`,
  `unmount_path`). `--layout` now defaults to `fat32`; `ntfs` is no longer a
  valid value.

### Verified against real hardware

The acceptance criterion throughout was **Windows Setup reaching disk
selection** — not the language screen, which proves only that the firmware
found a bootloader. Reaching disk selection is what proves Setup found and
accepted its installation source, split `.swm` parts included.

- Media written **from macOS**: a UEFI machine (with an unsplit `install.esd`
  and with a split `install.wim`) and a legacy-BIOS machine (Intel Atom N455
  netbook, AMI BIOS dated 2011), through Argos's own MBR and VBR.
- Media written **from Linux** (M5.1): a real Windows 10 22H2 ISO — 6.14 GB,
  `install.wim` 5.18 GB, so the splitter is genuinely exercised — to a
  physical 28.7 GB stick, booting a UEFI machine with `--layout fat32` and a
  legacy-BIOS machine with `--layout fat32-bios`.
- Both hosts, both firmwares, with no Windows machine, no `mkfs.ntfs`, no
  `ntfs-3g`, no FUSE and no vendored binary blob anywhere in the path.
- The stale-GPT scenario (#59) on real hardware: one stick written with the
  GPT layout and then the MBR layout still reached its installation source,
  which a surviving GPT would have prevented.

Still open: an installation carried through to completion rather than stopping
at disk selection, and Secure Boot (M5.3).

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
