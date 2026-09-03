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
  deferred to the backlog. (Phase 3 update: that deferral ended -- the FAT32 +
  WIM-split layout is now the *strategic replacement* for UEFI:NTFS, not a
  compatibility fallback; see `docs/plan-phase3-self-contained.md` §2. M3
  (#43) implemented the FAT32 layout as `--layout fat32`:
  `partition::windows::WindowsFat32Plan` (one Microsoft Basic Data partition
  -- deliberately *not* ESP-typed: firmware boots removable FAT32 media by
  scanning for `\efi\boot\bootx64.efi` regardless, and basic-data is what
  Rufus ships, the most-travelled path), formatted and populated via
  pure-Rust `fatfs` over `argos-privileged::partition_io::PartitionWindow`,
  a Read+Write+Seek view bounded to the partition's byte range of the open
  whole-device fd -- no `mkfs`, no mount, no partition device nodes, no
  partition-table reread, zero external processes in the write path. FAT32
  refuses >4GiB-1 files with a dedicated error (exit 26) until M2's WIM
  splitter slots into the copy pipeline -- it has since landed, and M5 put
  FAT32 media booting real UEFI *and* legacy-BIOS machines to Windows
  Setup's disk selection from both hosts. **M4.3 decision: NTFS is retired,
  not merely demoted** -- `fat32` is the only layout Argos produces (plus
  `fat32-bios` for legacy BIOS, M6); see the Status table below for the
  scope of what came out of the tree with it.)
- **`gptman`** (pure-Rust GPT), **`cdfs`** (ISO9660 reader, used here under
  the local dependency name `cdfs` but backed by the `newtua-cdfs` fork -- see
  below), and **`hadris-udf`** (pure-Rust UDF/ECMA-167 reader, added after W1
  validation showed real Windows media needs it -- see the correction below)
  are the only new dependencies this needs. (Phase 3 update: `hadris-udf`
  has since been replaced at runtime by Argos's own `image::udf` module --
  see the correction below and `docs/plan-phase3-self-contained.md` -- and
  survives only as a dev-dependency fixture generator.)
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
  `image::windows` therefore tries UDF first, falling back to `cdfs` only
  for genuinely ISO9660-only Windows-shaped images -- which in practice
  means this crate's own synthetic test fixtures, not real media. The UDF
  backend was originally [`hadris-udf`](https://github.com/hxyulin/hadris)
  (the reason the workspace's `rust-version` moved from 1.75 to 1.88;
  `gptman` stayed pinned to its 1.x line regardless); phase 3's M1 (backlog
  #40) replaced it with Argos's own `image::udf` module after `hadris-udf`'s
  whole-file-in-memory read caused a real OOM (#38) and its private extent
  resolution made streaming impossible to add from outside. `hadris-udf`
  remains a dev-dependency only, generating UDF test fixtures as an
  independent implementation `image::udf` is verified against.

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
  is reported as inconsistent and must be rewritten in full. The FAT32 Windows
  path goes one step further and *invalidates* what it leaves, because unlike
  DD mode its partial media would otherwise mount and look plausible.
- `image::isohybrid`: classifies an ISO from its first couple of sectors
  (embedded MBR signature + partition entry, El Torito boot catalog, a
  best-effort GPT/UEFI hint) into `Hybrid` / `ElToritoOnly` / `PlainData`.
  Only `Hybrid` is writable in DD mode.
- `image::udf` (phase 3, M1 / backlog #40): Argos's own minimal, read-only
  UDF/ECMA-167 reader with **streaming** file access -- anchor/volume
  descriptor sequence parsing (tag checksum + CRC verified on every
  descriptor), type-1 partition maps, File/Extended File Entries, short and
  long allocation descriptors, embedded data, sparse extents, and
  allocation-extent continuation chains. A file read materializes only the
  extent list and serves content in caller-sized chunks through a shared
  `Mutex`-guarded source, so a multi-GB `install.wim` costs a few MB of RAM
  to copy (measured: 3.5MB peak RSS streaming a 512MB file), not its own
  size. Anything outside scope (metadata partitions, extended ADs,
  non-2048-byte blocks) is refused with a clear error, never misread.
- `image::windows` (backlog #27, W1): recognizes an official Windows
  installer ISO by the presence of `bootmgr` + `sources/boot.wim` at its
  root, rather than fixed byte offsets, since a Windows ISO carries no
  embedded MBR/GPT to probe. Tries `image::udf` first, falling back to
  `cdfs` (see the phase 2 guiding decisions above, including the post-W1
  correction on why UDF has to come first). `WindowsIso` is a thin
  read-only wrapper (list files with their sizes, open one by path) over
  whichever backend recognized the image, reused by `windows_fat32` to copy
  the extracted files onto the FAT32 partition -- both backends stream, so
  the copy runs in constant memory.
- `image::checksum`: streaming SHA-256, used both to fingerprint the source ISO
  and (once E5/E6 land) to verify what was actually written.
- `preflight`: capacity and source/target-collision checks that run in the
  unprivileged process before the user is even asked to confirm anything --
  the same pattern balenaEtcher uses in its renderer process before handing
  work to its privileged sidecar. `check_windows_fat32_capacity` (phase 3 M3,
  backlog #43) is the Windows-write equivalent of `check_capacity`: it
  compares the device against `WindowsFat32Plan::total_bytes_required`
  instead of the raw ISO size, since the partition needs overhead margin and
  GPT/MBR structures on top of that. An earlier `check_windows_memory` guard
  (#38, found running the retired NTFS path's W6 against real hardware)
  briefly guarded the UDF backend's whole-file-in-memory read -- a real
  Windows 10 ISO's `install.wim` had pushed a 7.7GB-RAM machine into memory
  pressure severe enough for `systemd-oomd` to kill an unrelated process
  sharing `argos-helper`'s cgroup. It was retired together with the cost it
  guarded when `image::udf`'s streaming reader (phase 3 M1, #40) made the
  copy constant-memory; its exit code (25) stays reserved rather than reused.
- `partition::windows` (phase 3 M3/M6, backlog #43/#45): pure arithmetic, no
  disk I/O. `WindowsFat32Plan::new` (GPT) and `WindowsMbrPlan::new` (MBR, M6)
  lay out the single FAT32 partition -- a 1 MiB-aligned start (the same
  convention Windows Setup/Rufus/`parted` use), a sector-rounded size with a
  fixed overhead margin on top of the extracted (and, where needed,
  WIM-split) files' raw byte total (deliberately generous and uncalibrated;
  M5's real-hardware pass is what told us it didn't need adjusting) -- and
  `total_bytes_required` folds in the GPT/MBR structure overhead for the
  capacity preflight check above. `windows_fat32` turns either plan into a
  real partition table via `gptman`/`mbrman`.
- `verify` (phase 3 M3, backlog #43): `verify_windows_fat32_layout` and
  `verify_windows_file_hash` are the Windows-write path's counterpart to
  `verify_written_image` above -- deliberately *not* a reuse of it, since
  that function assumes one meaningful whole-device hash, and a partitioned
  write has none. Both are pure comparisons over plain data
  (`ObservedPartition` wraps a partition type GUID + region, carrying no
  `gptman` type -- only `argos-privileged`, which does the actual reading,
  links that crate), so they're unit-tested the same way as everything else
  here: no disk, no privilege.
  `argos_privileged::windows_fat32::execute_verify_windows_fat32` is what
  actually reads a real GPT/MBR and the FAT32 filesystem and calls into
  these.

### `argos-platform` / `argos-platform-linux`

`PlatformOps` is intentionally small and free of Unix-specific assumptions (no
`/dev/sdX` parsing baked into the trait) so a real Windows backend could
implement it later without the trait changing.

Three methods the NTFS write path added (backlog #27, W3) --
`reread_partition_table`, `mount_ntfs_partition`, `unmount_path` -- were
removed from the trait and every backend along with that path itself
(decision point M4.3): the FAT32 layout needs none of them, writing directly
into the partition's byte range of the open whole-device fd instead of
reading/formatting/mounting a filesystem node.

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

**Cancellation is wired end-to-end** (M7.5, backlog #35). `argos` keeps the
helper's stdin open for the whole write instead of closing it after the plan
line; a `SIGINT` handler writes `protocol::CANCEL_SIGNAL` into it, and a
watcher thread in `argos-helper` turns that byte -- or the pipe simply closing,
which is what a parent that died outright leaves behind -- into a
`CancelToken` the write loop checks on every buffer.

The helper **ignores `SIGINT` itself**. Ctrl-C in a terminal goes to the whole
foreground process group and the privilege broker leaves the helper in it, so
the default disposition would kill it mid-write, before the write path could
act on the cancellation at all. Cancellation therefore has exactly one
channel, and the helper stays alive long enough to use it -- which matters
because the FAT32 path does real work on the way out: it destroys the volume's
boot sectors, so half-written media cannot be mistaken for good media.
`SIGKILL` still cannot be caught, and media left behind that way is exactly
what `ArgosError::Cancelled` has always described.

**The NTFS write path (backlog #27, W3/W4) was retired at decision point
M4.3**, once the FAT32 layout below was validated on real hardware from both
hosts, on both firmwares (see the phase 2 guiding decisions above, and
`docs/plan-phase3-self-contained.md` §5 / `docs/plan-linux-validation.md` for
the record of that validation). `windows::execute_write_windows_image`/
`execute_verify_windows_image`, the `WindowsPartitionPlan` two-partition
layout, the vendored `uefi-ntfs.img` boot image, the `mkfs.ntfs`/`ntfs-3g`
shell-outs, and the three `PlatformOps` methods that existed only to support
them (`reread_partition_table`, `mount_ntfs_partition`, `unmount_path`) are
all gone from the tree, not merely superseded in the CLI's default. Everyone
sending Argos a `Plan::WriteWindowsImage`/`VerifyWindowsImage` with
`WindowsLayout::Fat32`/`Fat32Bios` -- the only variants left -- reaches
`windows_fat32::execute_write_windows_fat32`/`execute_verify_windows_fat32`
instead; an old plan JSON with no `layout` key at all now defaults to
`Fat32` rather than the retired `Ntfs` (`protocol.rs`'s backward-compatible
`#[serde(default)]` still parses it, it simply means something different now
than it did before M4.3, since the old meaning no longer exists to preserve).

`windows_fat32::execute_write_windows_fat32` is `execute`'s Windows-write
equivalent, dispatched via the same `WriteWindowsImage` `Plan` variant. In
one privileged elevation -- CONTRIBUTING.md's scoped exception to this
crate's "keep it minimal" rule covers exactly this -- it re-validates the
device (`validate_refreshed_device_for_windows_write`), re-classifies and
re-lists the source ISO itself (never trusting the plan's idea of what's on
it), plans which files copy verbatim and which need WIM-splitting
(`plan_copy_actions`, phase 3 M2.3/M3.5, backlog #42/#43), builds a
`WindowsFat32Plan`/`WindowsMbrPlan` (GPT or MBR, per `plan.layout`), writes
the partition table via `gptman`/`mbrman`, then formats and populates the
one FAT32 partition through `fatfs` over a
`partition_io::PartitionWindow`/`SizedDevice` -- a `Read+Write+Seek` view
bounded to the partition's byte range of the open whole-device fd. No
`mkfs`, no mount, no partition device nodes, no partition-table reread: the
whole write is this process talking to one file descriptor. Every file is
hashed in the same pass it's copied (`image::checksum::copy_and_hash`); an
oversized `install.wim` streams through `image::wim`'s splitter into `.swm`
parts instead (UDF read -> splitter -> `fatfs` write, still one pass).
`repair_directory_entries` runs after the copy -- see below for why.
Exercised against a real file-backed loop device in
`crates/argos-privileged/tests/write_windows_fat32.rs` (root + `losetup`
gated, no `mkfs.ntfs`/`ntfs-3g`/`--partscan` needed -- that difference from
the retired NTFS path *is* M3's acceptance criterion), and validated on
real hardware from both hosts (see above).

`windows_fat32::execute_verify_windows_fat32` is the same write path's
verification counterpart, dispatched via `VerifyWindowsImage`, following the
same read-only posture `execute_verify`'s `VerifyPlan` established (no
TOCTOU refusal window, no `expected_serial`/`expected_size_bytes`). It
re-derives the expected layout from the source ISO exactly like the write
path does, reads the real GPT/MBR off the device and checks it against the
plan (`argos_core::verify::verify_windows_fat32_layout`/
`verify_mbr_layout`), then reads the FAT32 filesystem back -- read-only,
still no mount -- and hashes every file `WindowsIso` lists against a fresh
read of the source ISO (`argos_core::verify::verify_windows_file_hash`, one
call per file).

**`fatfs`'s directory-entry defects, and why the repair pass stays**
(phase 3 L4, backlog #56). `fatfs` 0.3.6 writes two things the FAT
specification forbids: long-filename entries in front of `.` and `..`, which
must be a directory's first two entries, and a `..` pointing at the root's
real cluster where the specification requires zero when the parent is the
root. Both are repaired after the copy by
`windows_fat32::repair_directory_entries` rather than avoided at the source.

The decision was between tracking an upstream fix, vendoring a patch, and
keeping that repair pass. **The repair pass stays**, for now, because the
upstream fix exists but is unreleased: crates.io serves 0.3.6, while
[`rust-fatfs`](https://github.com/rafalh/rust-fatfs) master -- an unreleased
0.4.0, actively maintained -- already fixes both defects, each exactly where
the analysis in #56 pointed (`create_dir` asks `is_root_dir()` before filling
in `..`; `write_entry` skips the long-name entries for `.` and `..`, its own
comment noting they "need to be at the first two slots"). Verified rather than
read: the same reproduction program compiled against each version writes `..`
as cluster 2 under 0.3.6 and cluster 0 under master, and `fsck.vfat -n` exits
1 on the first and 0 with clean output on the second. Master also initializes
the FSINFO free-cluster count, which is why our media still draws that one
remaining (spec-legal, status-neutral) `fsck` note.

So there is nothing to contribute upstream and nothing to vendor -- only a
release to wait for. Adopting master early is not a version bump: it replaced
the I/O surface with its own `IoBase`/`ReadWriteSeek` traits (`std` types go
through `StdIoWrapper`), so `windows_fat32` would need reworking. When 0.4.0
ships, `repair_directory_entries` and the FSINFO note should go with it, and
the FAT conformance tests on both hosts are what will say whether they can.

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
tagged `Plan` (`Write`/`Verify`/`WriteWindowsImage`/`VerifyWindowsImage`)
rather than always a `WritePlan`.

Both commands classify the image DD-mode-first (backlog #27, W5): if
`image::classify` doesn't recognize a hybrid Linux image, they try
`image::windows::classify` next, and only fall through to `UnsupportedIso`
if neither matches. On a Windows-installer match, `write` computes the same
`WindowsPartitionPlan` `execute_write_windows_image` will independently
recompute (purely for display -- the privileged side never trusts it) and
shows the two-partition layout it's about to create in place of a single
image size, before the same retype-the-device-path confirmation; `--no-verify`
has no effect on a Windows write, since `execute_write_windows_image` never
verifies inline (see its own doc comment for why splitting that into a
second privileged call wasn't worth it) -- `write` prints a reminder to run
`argos verify` afterward instead. Neither command attempts a Windows
image on a non-Linux host at all: both check `cfg!(target_os = "linux")`
themselves, before any preflight work or confirmation prompt, and fail with
a specific `WindowsImageRequiresLinux` error rather than only discovering
`NotImplemented` deep inside `argos-helper` after an elevation prompt.

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
| Windows ISO support (backlog #27) | W1-W5 implemented: W1 (`image::windows`: UDF-first/ISO9660-fallback detection + read-only file-tree wrapper -- corrected mid-implementation after real-media testing showed official Windows ISOs are UDF bridges, not plain ISO9660), W2 (`partition::windows::WindowsPartitionPlan`: two-partition layout arithmetic + `preflight::check_windows_capacity`), W3 (`argos-privileged::windows`: real GPT via `gptman`, vendored UEFI:NTFS boot image, `mkfs.ntfs`/`ntfs-3g` shell-outs, per-file copy+hash), W4 (`execute_verify_windows_image`: GPT layout + boot partition + per-file hash verification), and W5 (`argos write`/`argos verify` both classify DD-mode-first then try the Windows-installer shape, showing the two-partition layout before confirming, refusing early and honestly on non-Linux hosts). W1 confirmed end-to-end (classify, list 906 files, extract and byte-verify individual files including a 5.18GB `install.wim` listed correctly) against a real, official Microsoft Windows 10 22H2 ISO; W5's classification/layout/preflight logic re-confirmed against that same real ISO (correctly routed as non-DD/Windows-installer, correct two-partition layout and capacity pass/fail at plausible USB stick sizes). W2-W4 unit-tested; W3/W4's real-loop-device integration tests (root/`losetup`/`mkfs.ntfs`/`ntfs-3g`-gated) confirmed passing for real in CI. First real-hardware W6 attempt (real Windows 10 ISO to a physical USB drive) surfaced a real memory-exhaustion bug (#38, `install.wim`'s whole-file-in-memory UDF read plus a memory-constrained machine OOM-killed an unrelated process); first mitigated with a `check_windows_memory` preflight refusal, then fixed for real by `image::udf`, Argos's own streaming UDF reader (phase 3 M1, #40 -- constant-memory copy confirmed at 3.5MB peak RSS streaming a 512MB fixture file; the preflight guard and `hadris-udf` runtime dependency were retired with it). `image::udf` since re-validated against **both** real official ISOs (M1.5): Windows 10 22H2 (5.18GB `install.wim`) and Windows 11 25H2 (7.58GB `install.wim`, checksum-verified against Microsoft's published SHA-256), each streamed at **3MiB peak RSS** with a digest byte-identical to macOS's own native UDF driver reading the same file. Phase 3 M3 (#43) added the pure-Rust FAT32 single-partition layout behind `--layout fat32|ntfs` (`WindowsFat32Plan`, `PartitionWindow`, `argos-privileged::windows_fat32`): write+verify round-trip covered by unit tests over plain files and a root-gated loop-device integration test needing only `losetup` -- no `mkfs.ntfs`/`ntfs-3g`/`--partscan`. Phase 3 M2 (#42) added `image::wim`, Argos's own WIM reader/splitter: it redistributes whole stored resources into `.swm` parts without ever decompressing or re-encoding (so the lookup table's SHA-1s stay valid by construction, and no XPRESS/LZX codec is needed), and is wired into the FAT32 copy as a stream (UDF -> splitter -> `fatfs`, hashing in one pass). Validated against wimlib as an external oracle -- including `wimlib-imagex apply` reproducing a source tree byte for byte from our parts -- and against **both real ISOs**: Windows 10 22H2 (71824 lookup entries, 2 parts of 3.98GB + 1.16GB, 1.3s) and Windows 11 25H2 (95219 entries, 7.06GB of resources into 3 parts of 3.46GB + 3.98GB + 0.08GB, 2.3s) -- every part under FAT32's 4GiB-1 limit, `plan_part_sizes` predicting each size exactly before a byte was read, and `wimlib-imagex verify` passing over all 11 images and every byte of file data in both cases. M4 (#34) then enabled the whole FAT32 path on **macOS**, superseding that issue's original macFUSE/`ntfs-3g` route entirely -- with no `mkfs`, no mount and no partition device nodes, nothing in the path is platform-specific. Two real macOS device-node quirks were found by running it against a real `hdiutil`-attached disk and are handled in `argos-privileged::partition_io`: `/dev/diskN` reports 0 for `SEEK_END` (which `gptman` needs to lay out a new GPT -- `SizedDevice` answers it from the already-validated device size), and it rejects `fcntl(F_FULLFSYNC)`, which `File::sync_all` maps to on macOS, with `ENOTTY` (`sync_device` falls back to plain `fsync(2)`, only on that exact errno). Full FAT32 write+verify passes on macOS via `hdiutil` with no macFUSE, no ntfs-3g and no root. **M5 real-hardware result (partial)**: media written from macOS to a physical USB stick booted a real UEFI machine to the Windows Setup start screen. That validates two decisions that until then were only arguments: the M3.2 choice of a Microsoft Basic Data type GUID over an ESP (firmware found and ran `efi/boot/bootx64.efi` on a basic-data partition, as Rufus's media does), and that the FAT32 `fatfs` writes is readable by real firmware rather than only by our own reader. Getting there also surfaced three bugs no automated test had caught: the CLI kept its own pre-splitter 4GiB check and so refused real Windows media the helper could write; `argos verify` opened the disk read-write while macOS had auto-mounted the fresh partition (`EBUSY`); and a write died mid-copy with `EBUSY`, apparently from that same auto-mount, now guarded by an exclusive (`O_EXCL`) open -- a fix that is **inferred rather than reproduced**, since `hdiutil` images are exempt from disk arbitration and never auto-mount. Still pending: taking Setup past disk selection (the acceptance criterion that proves the split `.swm` is accepted -- the machine tested could not be installed to), M5.1 (Linux host), M5.3 (Secure Boot), and the M4.3 decision on retiring the NTFS layout. **M6 (BIOS/MBR) is next and is not optional**: producing media for old lab machines is the use case that motivated the project. Its M6.1 decision is settled -- Argos writes its own MBR and FAT32 boot records from source under MIT/Apache, declining a GPL relicense that would have allowed porting `ms-sys`'s field-tested (but binary-blob) records. **M6 is now implemented and validated on real BIOS hardware.** M6.2-M6.5 (#45) added `WindowsMbrPlan`, Argos's own MBR boot code (279 of the 440 bytes available) and FAT32 VBR (418 of 420), both written from scratch in 16-bit assembly, plus a QEMU/SeaBIOS boot-chain test that boots media the product's own write path produced. `--layout fat32-bios` media written **from a Mac** then booted a real legacy-BIOS machine (Intel Atom N455 netbook, AMI BIOS dated 2011) through Argos's MBR, Argos's VBR, `bootmgr` and WinPE to Windows Setup's **disk selection** -- the acceptance criterion that had been pending, and the one that proves the split `.swm` is accepted by Setup itself. The same criterion was also met on a real UEFI machine, with both an unsplit `install.esd` and a split `install.wim`. That closes M2, M3, M4 and M6 against real hardware, from a host with no Windows machine, no `mkfs.ntfs`, no `ntfs-3g` and no vendored binary blob anywhere in the path. Getting there cost several rounds of lab testing against a symptom -- WinPE showing the volume as FAT32 with **no drive letter**, and Setup reporting a missing media driver -- that six separately-confirmed real defects failed to explain (zeroed CHS in the MBR entry, a desynchronized backup boot sector, a previous bootloader surviving a GPT write, `.`/`..` entries violating the FAT spec (#56), a fixed volume serial, and `BPB_HiddSec` left at 0 on the GPT path). The actual cause was found by dumping a written stick sector by sector and comparing it against Rufus-written media (`tools/mediadiff.py`): `mbrman` writes sector 0 and nothing else, so a stick previously written with `--layout fat32` kept its **entire GPT** -- primary header at LBA 1, entry array behind it, backup header in the device's last sector, every CRC still validating -- underneath an MBR whose first entry is a bootable FAT32 partition rather than the protective `0xEE` a GPT requires. Windows will not hand a volume on a disk in that state a drive letter, and the media still *boots*, which is what made it so hard to localize. `write_mbr_partition_table` now erases both GPT copies and `verify_mbr_layout` refuses media that still carries one (#59). It is also why emulation never reproduced the failure: the QEMU harness builds its media in a freshly truncated file, which has no stale GPT to leave behind -- only a recycled device reproduces it, and every lab stick had been written with the GPT layout first. **M5.1 (Linux host) is now closed too**: a real Windows 10 22H2 ISO written from a Linux host (Arch, kernel 7.1) to a physical SanDisk 28.7GB stick booted to Windows Setup's **disk selection** on a real UEFI machine with `--layout fat32` and on a real legacy-BIOS machine with `--layout fat32-bios` (2026-09-03). The FAT32 path is therefore validated against real hardware from **both** supported hosts and on **both** firmwares, which is the whole of what phase 3 set out to prove. That run also exercised the #59 recycled-stick scenario on real hardware -- the same stick took the GPT layout first and the MBR layout second, and Setup still reached its installation source, which a surviving GPT would have prevented. **M4.3 is decided: the NTFS layout is retired, not merely demoted.** With M5.1 closing the boot criterion on both hosts and both firmwares, there was no longer a reason to carry `mkfs.ntfs`/`ntfs-3g` shell-outs, the vendored `uefi-ntfs.img` blob, the two-partition `WindowsPartitionPlan`, or the three NTFS-only `PlatformOps` methods -- keeping "NTFS has no 4GiB file limit" as a reason to keep the path stopped being persuasive once M2's splitter made that limit a non-issue for FAT32 too. `windows.rs`, `assets/`, `write_windows_image.rs`, and every NTFS-only code path across `argos-core`/`argos-platform*`/`argos-privileged`/`argos-cli` are gone from the tree; `--layout`'s default is now `fat32`, and `ntfs` is no longer a valid value (a plan JSON with no `layout` key -- from before the field existed -- now parses as `fat32`, the only meaning left for that default). Still pending: an installation carried to completion rather than stopping at disk selection, and M5.3 (Secure Boot) |
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
