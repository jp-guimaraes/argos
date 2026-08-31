# Argos architecture

Argos creates bootable installer USB drives, in the spirit of
[Rufus](https://github.com/pbatard/rufus) but built to run on Linux and macOS
hosts. This document describes the design and tracks the current
implementation status against the product backlog. See the repository history
for the full backlog (epics E0-E11) that this was planned from.

## Guiding decisions (v1)

- **Language**: Rust.
- **Interface**: CLI first. The crate boundaries below are chosen so a future
  GUI can sit on top of `argos-core` + `argos-platform` without changes to
  either.
- **Image scope**: Linux ISOs only, written byte-for-byte ("DD mode") when the
  ISO is an isohybrid image -- which covers essentially every mainstream
  distro today, since the image already embeds a valid MBR/GPT and BIOS/UEFI
  bootloaders. Non-hybrid ISOs are refused with a clear error rather than
  half-supported. Windows images (`install.wim` splitting, NTFS, UEFI:NTFS)
  are an explicit phase 2, not attempted here.
- **Hosts**: Linux and macOS. Windows-as-host is a stub crate only (see below).
- **Top priority**: never write to the wrong disk. Every device-safety
  decision layers multiple independent signals rather than trusting a single
  OS-reported flag, and the negative test suite (never write a disk flagged as
  a system disk) is treated as the most important test in the project.
- **No shelling out** to `dd`/`parted`/`sgdisk`/`mkfs`. `gptman`/`mbrman`/
  `fatfs` (pure Rust) cover partition-table and FAT needs for phase 2; the only
  accepted external-process calls are unmount/eject helpers
  (`umount`/`eject` on Linux, `diskutil` on macOS).

## Guiding decisions (phase 2: Windows ISO support, backlog #27)

v1's "Linux ISOs only, DD mode" scope (above) was always meant to be revisited
once it shipped. Windows install-media support breaks the "Argos never
creates/alters a partition table" invariant v1 relied on, so -- per the
project's own rule that this needed an explicit architecture decision before
being touched -- these were decided in a 2026-08-30 planning session, ahead of
implementation, and are tracked as backlog issue #27 (sub-epics W1-W6):

- **Implementation/test platform: Linux first.** The highest-risk new piece is
  creating a real NTFS partition, which is native and mature on Linux
  (`ntfs-3g`/`mkfs.ntfs`) but on macOS depends on `ntfs-3g` via Homebrew
  running on macFUSE (kext approval, historically fragile) -- not reliably
  testable there. The macOS backend is deferred; `argos-core`'s planning logic
  (image classification, partition-plan arithmetic) stays host-agnostic from
  the start, same as v1's split.
- **FAT32 4GB limit (`install.wim`/`.esd`) workaround: UEFI:NTFS**, Rufus's
  current method -- a small FAT32 boot partition (loads the UEFI:NTFS driver)
  plus a large NTFS partition holding the Windows files untouched, no
  splitting. The boot partition is always an exact copy of
  [`pbatard/uefi-ntfs`](https://github.com/pbatard/uefi-ntfs)'s pre-built,
  Secure-Boot-signed `.img` (the same artifact Rufus vendors as
  `res/uefi/uefi-ntfs.img`), so it's a plain `dd` of a vendored binary --
  `argos_core::write::dd_mode` already covers that, and no FAT32-writing crate
  is needed. Splitting `install.wim` into `.swm` (`wimlib`, Rufus's older
  method, useful for very old BIOS-only compatibility) is deliberately
  deferred to the backlog.
- **`gptman`** (pure-Rust GPT), **`cdfs`** (ISO9660 reader, used here under
  the local dependency name `cdfs` but backed by the `newtua-cdfs` fork -- see
  below), and **`hadris-udf`** (pure-Rust UDF/ECMA-167 reader, added after W1
  validation showed real Windows media needs it -- see the correction below)
  are the only new dependencies this needs.
- This relaxes v1's "no shelling out" rule specifically to call
  `mkfs.ntfs`/`ntfs-3g` as an external process on Linux to format and mount
  the NTFS partition -- the same posture v1 already accepted for
  `unmount`/`eject` helpers. Creating the partition table itself stays
  pure-Rust (`gptman`).
- **`cdfs` dependency: the `newtua-cdfs` fork, not the canonical crate.** The
  canonical `cdfs` crate on crates.io hard-depends on `fuser` (FUSE bindings)
  and a `clap`-based mount binary, neither of which `argos-core` uses or wants
  as a transitive dependency. `newtua-cdfs` (maintained by the same team as
  The Unarchiver) is a "forced fork" that strips exactly that and nothing
  else, publishing its library under the same crate name (`cdfs`) and API --
  `argos-core`'s `Cargo.toml` depends on it as `cdfs = { package = "newtua-cdfs",
  ... }` so the rest of the codebase is unaffected by the rename.
- **Correction (post-W1 validation): real Windows installer media is UDF, not
  ISO9660 -- this is the norm, not a rare edge case.** The original planning
  above treated a UDF bridge as a risk unique to unusually large multi-edition
  images. Testing `image::windows` against a real, official Windows 10 22H2
  ISO (single edition, nothing unusual) during W1 disproved that: it's
  mastered as an ISO9660+UDF bridge, with the ISO9660 layer exposing only a
  stub `README.TXT` -- `bootmgr`, `sources/`, and everything else live in the
  UDF layer exclusively. `cdfs` (ISO9660-only) could not see any of it.
  `image::windows` now tries [`hadris-udf`](https://github.com/hxyulin/hadris)
  (pure-Rust ECMA-167/UDF, `read`+`std`+`sync` features) first, falling back
  to `cdfs` only for genuinely ISO9660-only Windows-shaped images -- which in
  practice means this crate's own synthetic test fixtures, not real media.
  This is also why the workspace's `rust-version` moved from 1.75 to 1.88
  (`hadris-udf`'s MSRV); `gptman` stayed pinned to its 1.x line regardless
  (already implemented and tested against it before this correction, and
  revisiting to gptman's newer 2.x/3.x -- now within reach MSRV-wise too --
  is unrelated cleanup).

## Crate layout

```
crates/
  argos-core/             # pure domain logic -- no direct disk/OS I/O
  argos-platform/         # the PlatformOps trait every backend implements
  argos-platform-linux/   # real implementation: sysfs + udev database + /proc/mounts + UDisks2 cross-check
  argos-platform-macos/   # real implementation: diskutil -plist + df
  argos-platform-windows/ # deliberate stub, proves the trait has no Unix bias, out of v1 scope
  argos-privileged/       # argos-helper: the one binary meant to run as root
  argos-cli/              # the `argos` binary
xtask/                    # developer tooling (currently unused placeholder)
```

`argos-core` never imports anything OS-specific for disk access; it receives
plain data (`Device`, byte streams, sizes) from whichever `argos-platform-*`
crate is selected for the current OS. That split is what makes ISO
classification, checksumming, and the safety judgement unit-testable with
ordinary files and in-memory buffers -- no root, no real hardware.

### `argos-core`

- `device`: the `Device` model and `Bus` enum, plus the single safety gate
  `Device::is_safe_to_write()` (requires: not a system disk, OS-reported
  removable, USB bus -- all three, never just one).
- `error`: `ArgosError`, a `thiserror` enum mapped to stable CLI exit codes.
- `progress`: `ProgressSink` trait and a `CancelToken` for cooperative
  cancellation. Cancelling never tries to "undo" a partial write -- the device
  is reported as inconsistent and must be rewritten in full.
- `image::isohybrid`: classifies an ISO from its first couple of sectors
  (embedded MBR signature + partition entry, El Torito boot catalog, a
  best-effort GPT/UEFI hint) into `Hybrid` / `ElToritoOnly` / `PlainData`.
  Only `Hybrid` is writable in DD mode.
- `image::windows` (backlog #27, W1): recognizes an official Windows
  installer ISO by the presence of `bootmgr` + `sources/boot.wim` at its
  root, rather than fixed byte offsets, since a Windows ISO carries no
  embedded MBR/GPT to probe. Tries `hadris-udf` first, falling back to
  `cdfs` (see the phase 2 guiding decisions above, including the post-W1
  correction on why UDF has to come first). `WindowsIso` is a thin
  read-only wrapper (list files with their sizes, open one by path) over
  whichever backend recognized the image, reused by W3 to copy the
  extracted files onto the NTFS partition.
- `image::checksum`: streaming SHA-256, used both to fingerprint the source ISO
  and (once E5/E6 land) to verify what was actually written.
- `preflight`: capacity and source/target-collision checks that run in the
  unprivileged process before the user is even asked to confirm anything --
  the same pattern balenaEtcher uses in its renderer process before handing
  work to its privileged sidecar. `check_windows_capacity` (backlog #27, W2)
  is the Windows-write equivalent of `check_capacity`: it compares the device
  against `WindowsPartitionPlan::total_bytes_required` instead of the raw ISO
  size, since a two-partition GPT layout needs more room than that.
- `partition::windows` (backlog #27, W2): pure arithmetic, no disk I/O.
  `WindowsPartitionPlan::new` lays out the UEFI:NTFS boot partition and the
  NTFS Windows partition -- 1 MiB-aligned starts (the same convention
  Windows Setup/Rufus/`parted` use), sector-rounded sizes, a fixed NTFS
  overhead margin on top of the extracted files' raw byte total (deliberately
  generous and uncalibrated; W6's real-hardware pass is what should tell us
  whether it needs adjusting) -- and `total_bytes_required` folds in the
  primary/backup GPT structure overhead for the capacity preflight check
  above. W3 turns this plan into a real GPT via `gptman`.

### `argos-platform` / `argos-platform-linux`

`PlatformOps` is intentionally small and free of Unix-specific assumptions (no
`/dev/sdX` parsing baked into the trait) so a real Windows backend could
implement it later without the trait changing.

Three methods added for the Windows write path (backlog #27, W3) --
`reread_partition_table`, `mount_ntfs_partition`, `unmount_path` -- are
Linux-only in practice: macOS returns `NotImplemented` for all three (see
the phase 2 guiding decisions above), and Windows-as-host already returns
`NotImplemented` for everything. `reread_partition_table` wraps the
`BLKRRPART` ioctl via `gptman::linux` (cfg-gated to `target_os = "linux"`,
since that module doesn't exist on other targets -- the one place this
crate needs a compile-time OS split rather than the runtime-graceful-failure
posture everything else here uses). `mount_ntfs_partition` shells out to
`ntfs-3g` against a derived partition device path (`mounts::partition_device_path`,
the reverse of the existing `whole_disk_of`) and returns a fresh `tempfile`
mountpoint; `unmount_path` shells out to `umount`.

The Linux backend enumerates disks by reading `/sys/block/*` directly (size,
removable flag, vendor/model) and cross-referencing the udev database at
`/run/udev/data/b<major>:<minor>` for a more reliable bus classification and
serial number, when udev has recorded the device. Reading the udev database as
flat text files -- rather than linking `libudev` via bindgen -- is a
deliberate v1 simplification: no extra system libraries needed to build.

That sysfs/udev verdict is then cross-checked against UDisks2 over D-Bus
(`udisks2.rs`, using `zbus`'s blocking API), when `udisksd` is reachable: this
is the "two sources, cross-referenced" defense in depth from the original
design notes, matching what desktop file managers show. The cross-check can
only push the result to be *more* conservative, never less -- if UDisks2
disagrees that a device is a removable USB disk, `os_reports_removable` is
cleared regardless of what sysfs/udev concluded on their own. When UDisks2
isn't running (headless servers, containers, minimal installs) or the D-Bus
call fails for any reason, `Udisks2Snapshot::fetch()` returns `None` and
enumeration falls back to sysfs/udev alone, unchanged from before this
existed. One non-obvious wrinkle found while wiring this up against this
machine's real `udisksd`: a single UDisks2 `Drive` object backs *several*
`Block` objects (the whole disk plus each of its partitions, all pointing at
the same `Drive`), so building the device-path lookup has to key on the block
device that lacks a `Partition` interface, not on the drive object path
itself (an earlier attempt collapsed a disk's own entry to whichever
partition happened to be processed last).

System-disk detection parses `/proc/mounts` and flags a disk as a system disk
if any of its partitions is mounted at `/`, `/boot`, `/boot/efi`, or `/home`.
This is a second, independent signal on top of the bus/removable check in
`Device::is_safe_to_write` -- a disk must clear both to be offered for
writing. Mount sources are resolved through any device-mapper stack in
between (`dm.rs`) before that check: LVM, software RAID, dm-crypt, and
multipath are all just `dm-N` block devices from the kernel's perspective,
and `/proc/mounts` only ever shows the top of that stack (e.g.
`/dev/mapper/vg-home`), never the physical partition underneath. Without this
resolution, a disk holding an LVM physical volume for `/home` -- a common
desktop Linux layout -- would never be recognized as a system disk, and an
ISO stored on such a filesystem would never trip the source/target collision
check either. The recursive walk itself is pure and unit-tested with fake
multi-level stacks (LVM, dm-crypt-under-LVM, striped volumes); reading the
real `/sys/block/*/slaves` relationship and resolving a `/dev/mapper/*`
symlink to its `dm-N` target are validated against a real `dmsetup` +
loop-device stack in `tests/dm_resolution.rs` (root-gated, mirroring
`argos-privileged`'s loop-device tests).

### `argos-platform-macos`

Enumerates disks via `diskutil list -plist` (top-level `WholeDisks`) plus one
`diskutil info -plist <id>` per disk, parsed defensively in `diskutil.rs` (a
missing/renamed key degrades to a documented default rather than panicking or
failing the listing -- covers the "diskutil's plist schema changes between
macOS versions" risk called out in the backlog). Synthesized APFS-container
pseudo-disks (`VirtualOrPhysical == "Virtual"`) are excluded outright, the
same way `argos-platform-linux` drops loop/dm/md/zram entries -- they aren't a
real, independently writable block device. An internal physical disk is
still *returned*, though: system-disk detection cross-references `diskutil
info -plist /`, walking through its APFS container to the physical store
backing it (the Apple Silicon case the backlog flags -- the boot volume's
`ParentWholeDisk` is a virtual container, not the real internal SSD, until
that walk happens) to flag the true system disk, while `RemovableMedia` and
`BusProtocol` (only `"USB"` maps to `Bus::Usb`) independently keep any
internal disk from passing `Device::is_safe_to_write` even if that
cross-reference ever fails. `unmount`/`eject` shell out to `diskutil
unmountDisk`/`diskutil eject`; `backing_device_of` shells out to `df -P`,
since (unlike `diskutil info`) it accepts an arbitrary file path rather than
just a device identifier or a volume's own mount point.

Verified end-to-end on a real Apple Silicon Mac, both internal and external:
unit tests on the parsing/decision logic (fixtures captured verbatim from
this machine's `diskutil` output, including a real external USB stick --
the previously-synthetic external fixture has since been confirmed against
real hardware), plus manual runs of the full `PlatformOps` surface against
this machine's real disks. `list_removable_disks` correctly enumerated the
internal disk (filtered of its three APFS-container pseudo-disks, flagged as
the system disk via the container walk-up, `is_safe_to_write() == false`)
alongside a plugged-in USB stick (`bus == Usb`, `is_safe_to_write() ==
true`); `refresh` re-resolved the USB stick by platform id; `backing_device_of`
resolved a real file's backing device; `unmount` cleanly unmounted both of
the USB stick's partitions (`diskutil unmountDisk`); `eject` logically
removed it from the OS (`diskutil eject`), confirmed by it disappearing from
`diskutil list`.

The DD-mode write itself (`argos-helper`, separate from E3's enumeration
scope) has since been verified too: `argos write` against a real physical
USB drive, using the real Alpine Linux 3.24.1 (`virt` flavor) ISO. The write
completed, `argos-helper`'s own post-write verification passed, and the
written bytes were independently re-read straight off `/dev/diskN` with
`sudo dd | shasum -a 256` (outside Argos entirely) and matched Alpine's
published SHA-256 exactly. macOS additionally popped its own "disk not
readable" dialog afterward (Disk Arbitration not recognizing the freshly
Linux-formatted disk) -- expected and harmless, same as any raw `dd`-written
Linux USB on macOS; dismissed with Ignore, never Initialize.

That Alpine `virt` drive booted on the test UEFI machine (a Microsoft
Surface) but hung part way through the kernel's own hardware
initialization, past the point Argos or the bootloader are involved --
consistent with `virt`'s minimal driver set meeting Surface's
non-standard firmware/controllers, not with a bad write (the byte-exact
re-hash above already rules that out). A second write to the same drive,
this time a real, official Ubuntu 22.04.5 LTS Desktop ISO
(checksum-verified against Canonical's published `SHA256SUMS` before
writing; `argos-helper`'s own post-write verification passing was this
run's write-correctness check, rather than a second external re-hash),
booted successfully on that same Surface -- full live GNOME session, not
just a bootloader handoff. Confirms the DD-mode write path end-to-end on
macOS with real, popular-distro media, matching what Linux already
confirmed with the same ISO family.

### `argos-platform-windows`

Returns `ArgosError::NotImplemented` from every method and has no public
constructor -- it exists only so `argos-cli` already depends on the trait and
picks a backend via `#[cfg(target_os = ...)]`, and so the trait itself is
proven not to have crept in a Unix bias. It cannot be instantiated, only
type-checked, since Windows-as-host is out of v1 scope entirely.

### `argos-privileged`

This is `argos-helper`, the one binary that runs as root. It reads a single
[`protocol::WritePlan`](../crates/argos-privileged/src/protocol.rs) as JSON on
stdin, re-resolves the target device by serial + size through the platform
backend (`protocol::validate_refreshed_device` is the TOCTOU guard: it refuses
the write if the device changed, disappeared, or now looks like a system disk,
regardless of what the plan claims), then runs the DD-mode write and (unless
`--no-verify` was passed) the post-write verification, reporting progress and
the outcome as one JSON `Event` per stdout line. It parses no ISO and talks to
no D-Bus/plist/UDisks2 API. The crate is split into a library (`protocol`,
reused by `argos-cli` to build plans and parse events) and the `argos-helper`
binary that is the only thing here meant to actually run privileged.

**Known gap**: cancellation is not wired end-to-end yet -- nothing outside the
helper process can currently trigger the `CancelToken` passed to the write
loop, so a running write cannot yet be interrupted cleanly from the CLI side.

`windows::execute_write_windows_image` (backlog #27, W3) is the UEFI:NTFS
write path's equivalent of `execute`, dispatched via a third `Plan` variant,
`WriteWindowsImage`. In one privileged elevation -- CONTRIBUTING.md's scoped
exception to this crate's "keep it minimal" rule covers exactly this -- it
re-validates the device (`validate_refreshed_device_for_windows_write`, the
`WriteWindowsPlan` counterpart to the TOCTOU guard above), re-classifies and
re-lists the source ISO itself (never trusting the plan's idea of what's on
it), builds a `WindowsPartitionPlan`, writes a real GPT via `gptman`
(protective MBR + one EFI System Partition entry + one Microsoft Basic Data
entry, using `partition::windows`'s type GUID constants), `dd`s the vendored
`uefi-ntfs.img` (embedded via `include_bytes!`; see
`crates/argos-privileged/assets/PROVENANCE.md` for its provenance) onto
partition 1, shells out to `mkfs.ntfs` to format partition 2 and to `ntfs-3g`
(via two new `PlatformOps` methods, Linux-only for now) to mount it, then
copies every file `image::windows::WindowsIso` lists onto it, hashing each
one in the same pass (`image::checksum::copy_and_hash`) rather than reading
it twice. A third new `PlatformOps` method wraps the `BLKRRPART` ioctl
(via `gptman::linux`) so the two new partitions show up as their own block
devices right after the GPT write, before formatting/mounting need them to.
Exercised against a real file-backed loop device in
`crates/argos-privileged/tests/write_windows_image.rs` (root + `losetup` +
`mkfs.ntfs` + `ntfs-3g` gated, same posture as backlog E9's loop-device
tests) -- not yet run against real hardware (that's W6) or wired into the CLI
(W5).

### `argos-cli`

`argos list` lists every physical disk visible to the current platform backend
and marks whether each is safe to write to.

`argos write` runs the full flow: refresh the target device, apply the
non-negotiable safety gate (a system disk is refused unconditionally; a
non-removable disk needs `--i-know-what-im-doing`), classify the ISO (refusing
anything that isn't a hybrid image), run the capacity and source/target
collision preflight checks, require the user to retype the exact device path
to confirm, then hand a `WritePlan` to `argos-helper` via `pkexec` (preferred
on Linux) or `sudo`, rendering its progress as an `indicatif` bar, then ejects
the device (`--no-eject` skips this) -- best-effort, the same posture the
`PlatformOps::eject` implementations already take internally, so a failed
eject warns rather than turning an otherwise-successful write into a
failure. Notably, `argos write` still does *not* call `unmount` before
opening the device -- see the Status table below.

`argos verify` re-runs post-write verification against a device without
writing again, hashing the ISO (the `Checksumming` phase) and comparing it
against a fresh read of the device (`argos_core::verify`), via the same
`argos-helper` elevation path `argos write` uses -- reading a raw device
needs the same privilege writing does. The `argos`<->`argos-helper` IPC is a
tagged `Plan` (`Write`/`Verify`) rather than always a `WritePlan`.

## Status

| Area | Status |
|---|---|
| Domain model, errors, progress/cancellation, ISO classification, checksum, preflight checks | Implemented, unit-tested |
| DD-mode write engine, post-write verification | Implemented, unit-tested |
| Linux disk enumeration | Implemented (sysfs + udev database, cross-checked against UDisks2/D-Bus when reachable) and LVM/RAID/dm-crypt-aware system-disk detection; pure parsing/resolution logic unit-tested, and the D-Bus and device-mapper glue each confirmed against this machine's real `udisksd` and a real `dmsetup` stack |
| macOS disk enumeration (`diskutil -plist`) | Implemented, unit-tested; manually verified end-to-end (list/refresh/unmount/eject/backing_device_of) against a real Mac, both its internal disk and a plugged-in USB stick |
| Privileged helper (`argos-helper`) | Implemented; end-to-end write+verify passes against a real file-backed Linux loop device, a real macOS `hdiutil`-attached disk image, and real physical USB drives on both Linux and macOS, including the TOCTOU re-validation guard in each case |
| `argos list` / `argos write` | Implemented and manually verified against real physical USB hardware on **both platforms**. Linux: first with a synthetic isohybrid-signed image, then with a real, official Ubuntu 26.04.1 Desktop ISO (checksum-verified against Canonical's `SHA256SUMS`) written byte-for-byte: device detection, confirmation flow, `pkexec` elevation, write, and post-write verification all passed, and the written bytes were independently re-hashed outside Argos and matched the official ISO checksum exactly; the resulting drive was confirmed to boot for real on **UEFI**. macOS: a real, official Alpine Linux 3.24.1 (`virt`) ISO (checksum-verified against Alpine's published `sha256`) written the same way, with the same independent `sudo dd \| shasum` re-hash matching exactly (that drive booted but hung mid-kernel-init on the UEFI test machine, a Surface -- consistent with `virt`'s minimal driver set, not a bad write); a second write of a real, official Ubuntu 22.04.5 LTS Desktop ISO (checksum-verified, `argos-helper`'s own post-write verification passing) to the same drive **booted successfully on that same Surface**, full live session. `argos write` now ejects the device automatically after a successful write (`--no-eject` to skip), and `argos-helper` now unmounts it immediately before opening it for write (the `Unmounting` phase) -- closing #20, the safe-open precondition the guiding decisions above call for, which nothing called until now. A no-op, not an error, when nothing was mounted. A third macOS write, a real official **Ubuntu 18.04.5 LTS** Desktop ISO (checksum-verified against Canonical's published `SHA256SUMS`) written to the same physical USB drive, was carried to a real, old BIOS/legacy machine (no UEFI at all) and **booted successfully in legacy MBR mode** -- confirming the last untested boot path for v1.0 (BIOS/legacy on Linux is still separately unconfirmed, but macOS-written media now covers both UEFI and BIOS). Progress feedback (`indicatif`) is currently invisible when stdout isn't a real terminal -- tracked separately. |
| `argos verify` (standalone) | Implemented. `execute_verify`'s core logic is confirmed for real against both a matching write and a mismatched device/ISO pair (`ChecksumMismatch`), via the E9 hdiutil-image tests on macOS (Linux loop-device equivalents written the same way, exercised by CI). The full CLI path -- device resolution, `sudo` elevation, progress bar, final printout -- was manually run end-to-end on this Mac against a real physical USB drive: `argos write` then a separate `argos verify` invocation both reported the same SHA-256 (`e73a6241...`), matching Alpine's published checksum. |
| Windows ISO support (backlog #27) | W1 (`image::windows`: UDF-first/ISO9660-fallback detection + read-only file-tree wrapper -- corrected mid-implementation after real-media testing showed official Windows ISOs are UDF bridges, not plain ISO9660), W2 (`partition::windows::WindowsPartitionPlan`: two-partition layout arithmetic + `preflight::check_windows_capacity`), and W3 (`argos-privileged::windows`: real GPT via `gptman`, vendored UEFI:NTFS boot image, `mkfs.ntfs`/`ntfs-3g` shell-outs, per-file copy+hash) implemented. W1 confirmed end-to-end (classify, list 906 files, extract and byte-verify individual files including a 5.18GB `install.wim` listed correctly) against a real, official Microsoft Windows 10 22H2 ISO. W2/W3 unit- and loop-device-tested (root/`losetup`/`mkfs.ntfs`/`ntfs-3g`-gated, not yet run in this environment); not yet run against real hardware (W6) or wired into the CLI (W5) |
| Packaging/distribution | GitHub Releases binaries (`x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin`) implemented via `.github/workflows/release.yml`, triggered by a `vX.Y.Z` tag push -- the cross-compile step (`x86_64-apple-darwin` from an Apple Silicon runner) and the packaging script were both confirmed by actually running them on this machine, though no tag has been pushed yet so the workflow itself hasn't run for real. crates.io publish and a Homebrew tap not started -- both need decisions/credentials only the project owner has (a crates.io account/token; a tap repo name and org). |

## Prior art consulted

- [Popsicle](https://github.com/pop-os/popsicle) (Rust, PopOS) -- closest prior
  art in the same language; confirmed D-Bus/UDisks2 as a viable Linux
  enumeration path and the CLI/core workspace split.
- [balenaEtcher](https://etcher.balena.io/) -- confirmed the privileged-sidecar
  pattern (`argos-helper`) and the pre-write capacity/source-target-collision
  checks now in `argos_core::preflight`.
- [Ventoy](https://www.ventoy.net/) -- reference disk layout (protective
  MBR + GPT + separate ESP) for a possible future multi-ISO / persistent
  partition mode, out of v1 scope.
