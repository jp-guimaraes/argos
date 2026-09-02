# Phase 3 plan: self-contained Windows media support (Linux + macOS, BIOS + UEFI)

Written 2026-09-01, from the analysis requested in `prompt.md`. This plan
assumes [`docs/architecture.md`](architecture.md) as the description of the
current state and does not restate it.

## 1. Where the project stands against the original goal

The original goal: a Rufus-like tool that, **from Linux or macOS**, produces
bootable installer USB drives for **both Linux and Windows 10/11 ISOs**,
targeting **both MBR/BIOS and GPT/UEFI** machines — with as little dependence
on third-party software as possible.

Concretely, what motivated the project: producing Windows install media for
the **old machines in the maintainer's teaching labs**, written from a modern
Mac or from the labs' own Linux machines, with no Windows host involved
anywhere. That matters for prioritisation and is easy to lose sight of when
reading the milestone list — "MBR/BIOS" above is not a completeness item, it
is the target hardware. See M6.

What is already met, and holds up well:

- Linux ISOs (isohybrid, DD mode) from both hosts, verified on real hardware
  on both UEFI and legacy BIOS boots. This half of the goal is **done**.
- The architecture choices are sound and should not be re-litigated:
  privilege-separated `argos-helper` with TOCTOU re-validation, the
  `core`/`platform`/`cli` split, pure-Rust `gptman` for partitioning,
  multi-signal device safety, the negative-test-first posture.
- "No shelling out to `dd`/`parted`/`sgdisk`/`mkfs.vfat`" already holds for
  everything v1 shipped.

What does **not** yet meet the goal:

| # | Problem | Where it lives today |
|---|---|---|
| P1 | **Memory exhaustion on Windows writes** ("alto uso de memória"): the UDF backend (`hadris-udf`) has no streaming read — `WindowsIso::open_file` holds one whole file in memory, and `install.wim`/`.esd` runs 4–5GB. Issue #38 (shipped) only *mitigates* this: `preflight::check_windows_memory` refuses the write cleanly instead of OOMing. The real fix — a streaming UDF reader — is tracked as issue #40, which also confirmed (by source inspection) that streaming **cannot** be built outside `hadris-udf`: its extent resolution and underlying reader are private. | `argos-core/src/image/windows.rs` (doc comment at ~line 213), `preflight::check_windows_memory` |
| P2 | **Windows writes are Linux-only**, because the chosen layout (NTFS data partition) needs external `mkfs.ntfs` + `ntfs-3g`. On macOS that would mean Homebrew `ntfs-3g` on macFUSE (kext approval, fragile — the "mac dependency problem" from the original request; issue #34 planned exactly that route). Today the CLI refuses early with `WindowsImageRequiresLinux`; it does not crash, but it also does not do the job. | `argos-privileged/src/windows.rs:475` (`mkfs.ntfs`), `argos-platform-linux/src/enumerate.rs:127` (`ntfs-3g`) |
| P3 | **Windows media is UEFI-only.** The UEFI:NTFS layout boots via an EFI driver; it provides no BIOS/legacy boot path at all. The goal explicitly includes MBR/BIOS targets for Windows. (Scope note: Windows 11 requires UEFI anyway, so this gap only matters for Windows 10 on pre-UEFI machines.) | `partition::windows`, `argos-privileged/src/windows.rs` |
| P4 | **A vendored third-party binary blob**: `uefi-ntfs.img` (pbatard's Secure-Boot-signed driver image) is embedded via `include_bytes!`. Unavoidable *for the NTFS layout*; goes away with it. | `argos-privileged/assets/` |
| P5 | Remaining OS-tool shell-outs: `umount`/`eject` (Linux), `diskutil`/`df` (macOS), `pkexec`/`sudo` for elevation. These are OS-bundled, not third-party installs — low harm, but replaceable with syscalls/FFI where cheap. | `enumerate.rs` in both platform crates |
| P6 | Cancellation is not wired end-to-end (a running write cannot be interrupted cleanly). Tracked as issue #35. | `argos-privileged`, known gap in `architecture.md` |

## 2. The one strategic decision this plan is built on

**Replace the NTFS + UEFI:NTFS layout with a pure-Rust FAT32 + WIM-split
layout as the primary Windows path.**

Rufus's *older* method: one FAT32 partition, with `install.wim` split into
`install.swm` parts under the FAT32 4GB file limit. Windows Setup detects
`install.swm` automatically; no NTFS, no EFI driver needed — FAT32 is what
UEFI firmware boots natively (`efi/boot/bootx64.efi` ships inside the ISO).

Why this route wins for *this* project's goal, even though Rufus's default
moved on to NTFS:

- **Kills every external program in the Windows path.** `fatfs` (pure Rust,
  already the planned FAT crate) can format and populate the partition by
  writing directly into the partition's byte range of the open device fd —
  no `mkfs`, no mount, no FUSE, no kernel filesystem driver at all.
- **That is exactly what unlocks macOS** (P2): the reason the macOS backend
  was deferred was `ntfs-3g`/macFUSE (issue #34's whole risk list). With no
  mount step, the macOS Windows path needs nothing v1 doesn't already have.
- **Removes the third-party binary blob** (P4) and its Secure Boot
  revocation exposure: FAT32 boots via Microsoft's own signed `bootx64.efi`
  from the ISO, no extra driver.
- **It is the only layout that can also serve BIOS boot** (P3) later: BIOS
  boot of Windows media means MBR + boot-sector code chain-loading
  `bootmgr` — which Rufus does over FAT32 or NTFS; FAT32 keeps that story
  single-filesystem.
- Cost: a WIM splitter must be written in Rust (wimlib is C/GPL; linking it
  would violate both the no-third-party goal and, practically, the MIT/Apache
  licensing). This is the plan's hardest new piece, priced accordingly below.

The NTFS path (W3–W5, already working on Linux) stays in the tree as a
`--layout ntfs` fallback until the FAT32 path is validated on real hardware,
then becomes non-default (or is retired — decision point M4.3).

Independently of the layout decision, the UDF streaming problem (P1) must be
fixed at the reader. Issue #40 established that `hadris-udf` cannot be
extended from outside; the options are an upstream contribution, a fork, or
an in-tree minimal read-only UDF module. Everything downstream (split, FAT32
copy, hashing) is then a constant-memory pipeline.

**What we deliberately do NOT rewrite**: `gptman`, `mbrman`, `fatfs`,
`sha2`, `zbus`, `plist` are pure-Rust *source* dependencies compiled into
our binaries — they are "códigos disponíveis em repositórios públicos", not
runtime third-party software. Rewriting them would add risk for zero
dependency reduction.

## 3. Work plan

Difficulty scale (per the request, so work can be routed):

- **C (critical)** — human or frontier model only: format/boot-level design,
  binary format parsing/writing, anything where a subtle bug bricks media or
  corrupts data silently.
- **H (high)** — strong model, human review required.
- **M (medium)** — mid-tier model, normal review.
- **L (low)** — simple model can do it; mechanical, well-specified, and the
  existing test suite catches mistakes.

Host scale (what a task needs to *run* — development itself is
platform-neutral Rust unless noted):

- **any** — pure logic + unit tests; runs on Linux or macOS (CI covers both).
- **Linux** — needs a Linux host (loop devices, `/proc`, ioctls, or the
  Linux-only integration-test jobs).
- **macOS** — needs a real Mac (`diskutil`, `hdiutil`, FFI against macOS
  frameworks).
- **real hw** — needs physical USB drives and/or a physical boot-test
  machine; human-executed by definition.

Dependency order: M1 → (M2 ∥ M3) → M4 → M5; M6 (BIOS) after M5; M7 anytime.
M6 is sequenced last but is **not** optional — see the priority note there.

Each milestone below is synchronized to a GitHub issue (see §5).

### M1 — Streaming UDF reads (fixes P1; GitHub #40)

| ID | Task | Diff. | Host |
|---|---|---|---|
| M1.1 | Decide the streaming route. Issue #40 already rules out "wrap it from outside" (extent resolution is private to `hadris-udf`); remaining options: upstream PR, fork (the `newtua-cdfs` pattern), or an in-tree minimal ECMA-167/UDF read-only module (anchor VDP → volume descriptor sequence → file set descriptor → ICB/file-entry walk → allocation descriptors). Only file *reading* needs streaming; directory listing can stay simple. | **C** (decision + spec work) | any |
| M1.2 | Implement the streaming reader chosen in M1.1. Must handle short + long allocation descriptors, inline/embedded data, multi-extent files, and 2KB-block ISOs; must be `Send` so the copy loop can stay as-is. | **C** | any |
| M1.3 | Rewire `WindowsIso::open_file` to return a true streaming reader for the UDF backend (the ISO9660 backend already streams); keep the public signature. | M | any |
| M1.4 | Retire `preflight::check_windows_memory`'s hard refusal once M1.3 lands (with streaming, the largest-file-vs-RAM comparison stops being meaningful); update `architecture.md` and close #38's follow-up notes into #40. | L | any |
| M1.5 | Tests: synthetic multi-extent UDF fixtures (the `hadris-udf` write feature stays as a dev-dependency fixture generator — an independent oracle), plus a manual pass against the real Windows 10 22H2 and a Windows 11 ISO asserting constant memory (RSS watermark) while extracting `install.wim`. | M | Linux (RSS check); fixtures: any |

**Acceptance**: extracting a 5GB `install.wim` uses O(buffer) memory; byte-hash matches the current whole-file read.

### M2 — Pure-Rust WIM splitter (enables FAT32 layout)

| ID | Task | Diff. | Host |
|---|---|---|---|
| M2.1 | WIM format reader: header, resource/lookup table, XML data block, integrity table. Reference: Microsoft's published WIM spec + wimlib's documentation (read for understanding; **no code copied** — wimlib is GPL). Read-only, over any `Read + Seek`. | **C** | any |
| M2.2 | Splitter: produce valid `.swm` parts under a configurable limit (default ~3.8GB, safely under FAT32's 4GiB−1) by distributing *whole compressed resources* across parts — no recompression, no resource re-encoding. Each part gets a corrected header (part number/total, `FLAG_HEADER_SPANNED`), its own lookup-table slice, and the XML/integrity blocks per spec. | **C** | any |
| M2.3 | Wire the splitter into the copy pipeline as a streaming sink: UDF stream (M1) → splitter → FAT32 writer (M3), hashing in the same pass. `install.esd` (solid LZMS, Windows 11 media): if resource-boundary splitting is impossible for a given solid block layout, detect it and refuse that ISO with a clear error rather than emitting a broken `.swm`. | H | any |
| M2.4 | Validation harness (dev-only, not shipped): cross-check our `.swm` output against `wimlib-imagex verify`/`split` output for the same input as an *external test oracle* (dev dependency only, never a runtime one). | M | Linux (wimlib packaged) |
| M2.5 | Unit fixtures: tiny hand-built WIMs (uncompressed + XPRESS) exercising part-boundary edge cases. | M | any |

**Acceptance**: `wimverify` passes on every part; a real Windows Setup run (M5.1) installs from the split media.

### M3 — Pure-Rust FAT32 target layout (replaces NTFS, fixes P2/P4)

| ID | Task | Diff. | Host |
|---|---|---|---|
| M3.1 | Partition-region I/O abstraction in `argos-privileged`: an `io::Read+Write+Seek` window over the open whole-device fd, bounded to one partition's byte range (offset + length from the `WindowsPartitionPlan`). Kills the need for `reread_partition_table` + per-partition device nodes + mounting in this path. | M | any |
| M3.2 | New single-partition GPT plan variant in `partition::windows`: one FAT32 basic-data partition (plus the ESP-flagging decision: mark the FAT32 partition itself ESP vs. basic-data — research what firmware accepts; Rufus marks it basic-data with the whole drive still booting. Record the decision). | M | any |
| M3.3 | Format the partition FAT32 via `fatfs::format_volume` on the M3.1 window; copy the ISO file tree (with `install.wim` replaced by M2's `.swm` parts) via `fatfs`, streaming + hashing per file. | M | any |
| M3.4 | Verify path: read the GPT + FAT32 back (`gptman` + `fatfs` read-only over M3.1), per-file hash against a fresh read of the ISO — mirror of `execute_verify_windows_image` for the new layout. | M | any |
| M3.5 | CLI: `--layout fat32\|ntfs` (default `fat32` once M5 validates it), plan display, and removal of the Linux-only gate for `fat32` (see M4). | L | any |
| M3.6 | Loop-device integration test mirroring `write_windows_image.rs` for the FAT32 layout — note it needs **no** root-only `mkfs.ntfs`/`ntfs-3g` gating, only `losetup`; plus the macOS variant via the existing `hdiutil` test pattern. | M | Linux + macOS |

**Acceptance**: existing W3/W4-style integration tests pass for the FAT32 layout on a loop device, with zero external processes spawned in the write path.

### M4 — macOS Windows-write enablement (supersedes the macFUSE route of GitHub #34)

| ID | Task | Diff. | Host |
|---|---|---|---|
| M4.1 | Lift `WindowsImageRequiresLinux` for the FAT32 layout; keep it for `--layout ntfs`. Ensure the macOS pre-write `diskutil unmountDisk` path covers the Windows flow (it already does for DD mode), and that `diskarbitrationd` doesn't remount mid-write. | M | macOS |
| M4.2 | `hdiutil`-image integration test on macOS for the full FAT32 Windows write+verify (same pattern as the E9 tests). | M | macOS |
| M4.3 | Decision point: once M5.2 passes, demote or retire the NTFS layout (and with it `mkfs.ntfs`, `ntfs-3g`, the vendored `uefi-ntfs.img`, and the three Linux-only `PlatformOps` methods W3 added). | L (the removal) / human (the decision) | any |

### M5 — Real-hardware validation (humans only)

| ID | Task | Diff. | Host |
|---|---|---|---|
| M5.1 | Linux host: real Windows 10 + Windows 11 ISO → physical USB via FAT32 layout; verify; boot Windows Setup on a real UEFI machine, proceed past disk selection (proves `.swm` is accepted). This is the W6 retry, now with M1 in place. | human | Linux + real hw |
| M5.2 | macOS host: same, written from the Mac. Boot-testing still happens on the same PC target (Apple Silicon can't boot Windows installers — #34 already recorded this). | human | macOS + real hw |
| M5.3 | Secure Boot on: confirm the FAT32 media boots with Secure Boot enabled (expected to, via Microsoft's own `bootx64.efi`). | human | real hw |

### M6 — Windows on BIOS/MBR (fixes P3)

**Priority corrected (2026-09-01).** This milestone was originally filed as
"optional, last", on the reasoning that BIOS-only machines are pre-2012 and
Windows 10 is out of support. That reasoning is sound in general and *wrong
for this project*: the maintainer's motivating use case is producing Windows
install media **for the old machines in the teaching labs he runs**, written
from a modern Mac or from the labs' own Linux machines, without needing a
Windows host anywhere. Those old machines are the target, not an edge case.
So M6 is not a nice-to-have — it is what makes the tool serve the purpose it
was built for. (It stays sequenced after M5, which is about not building on
an unvalidated foundation, not about M6 being optional.)

Scope note unchanged: Windows 11 requires UEFI and TPM 2.0, so BIOS-only
hardware means Windows 10 media specifically.

**Cheap check before building any of this**: many 2012–2015 machines expose
UEFI with a legacy CSM, and boot legacy only by default. If the lab machines
offer a `UEFI:`-prefixed entry in their boot menu, the existing FAT32 layout
already serves them and M6 is unnecessary. Confirm before investing.

#### M6.1 — decision: **write our own boot records** (settled 2026-09-01)

BIOS boot of Windows media needs two pieces of native x86 code that no ISO
ships and that this project does not yet have: MBR boot code (~446 bytes)
that finds the active partition and loads its boot sector, and a FAT32
PBR/VBR that parses FAT32 well enough to find and load `bootmgr`.

The three options were (a) write both from scratch, (b) adopt a
permissively-licensed implementation, (c) declare Windows-on-BIOS out of
scope.

- **(b) does not exist in practice.** The field implementations are all
  copyleft: Rufus (GPLv3) and its boot records, [`ms-sys`](https://ms-sys.sourceforge.net/)
  (GPL, same author, and the one that carries exactly the needed "FAT32
  partition PE boot record — for USB install and recovery"), Syslinux,
  FreeDOS, GRUB, Ventoy. Permissively-licensed boot code exists but boots
  BSD, not `bootmgr`.
- **(c) is ruled out by the use case above.**
- **Relicensing to GPL was explicitly considered and declined.** The
  maintainer is sole copyright holder of every commit, so it was legally
  available, and it would have reduced M6 from "write boot sector assembly"
  to "port field-tested code". It was declined to keep the crates
  permissively reusable — `argos-core` carries the streaming UDF reader and
  the pure-Rust WIM splitter, which have value beyond Argos — and because
  `ms-sys`'s boot records are **binary blobs**, so adopting them would
  reintroduce exactly the vendored-blob problem (P4) that replacing the
  UEFI:NTFS layout just eliminated.

**Decision: (a).** Argos writes its own boot records, from source, under
MIT/Apache. This keeps the project's defining property — everything on the
media is built from code in this repository — intact for the BIOS path too.

Accepted cost: this is the highest-risk code in the project. A wrong byte in
a boot sector produces "no bootable device" with no diagnostic, on hardware
that cannot be single-stepped. It must be developed against emulation
(QEMU/Bochs with a BIOS, where the boot process *can* be traced) before ever
reaching real hardware.

| ID | Task | Diff. | Host |
|---|---|---|---|
| M6.2 | MBR partition plan variant: `mbrman` instead of `gptman`, single FAT32 partition marked active/bootable, matching what the GPT plan already computes. | H | any |
| M6.3 | MBR boot code (~446 bytes, 16-bit x86): scan the partition table for the active entry, load its first sector to 0x7C00, jump. The simpler of the two — the algorithm is fully specified and has no filesystem knowledge. | **C** | any |
| M6.4 | FAT32 VBR: parse the BPB, walk the FAT and root directory to locate `bootmgr`, load it, hand off per the documented boot protocol. The hard piece. | **C** | any |
| M6.5 | Emulation harness: boot the produced media under QEMU with a legacy BIOS (SeaBIOS) in CI, asserting it reaches `bootmgr` — so boot-record regressions are caught by a test rather than by a dead lab machine. | H | any |
| M6.6 | Real pre-UEFI hardware validation: Windows 10 media written from macOS and from Linux, booted on an actual lab machine, taken past disk selection. | human | real hw (pre-UEFI) |

### M7 — Shell-out reduction + robustness (independent; good simple-model work)

| ID | Task | Diff. | Host |
|---|---|---|---|
| M7.1 | Linux `umount` shell-out → `libc::umount2` syscall (`enumerate.rs:82,143`). | L | Linux |
| M7.2 | Linux `eject` shell-out → ioctl sequence (`BLKFLSBUF` + SCSI START STOP via `SG_IO`, fallback `CDROMEJECT`). | M | Linux |
| M7.3 | macOS `df -P` shell-out → `libc::statfs` FFI (`enumerate.rs:191`). | L | macOS |
| M7.4 | macOS `diskutil` → DiskArbitration/IOKit FFI. **Deliberately deprioritized**: `diskutil` ships with macOS (not third-party), the plist parsing is battle-tested, and the FFI surface is large. Do last, or never. | H | macOS |
| M7.5 | Wire cancellation end-to-end (CLI signal → helper `CancelToken`), closing GitHub #35. | M | any |
| M7.6 | `pkexec`/`sudo` elevation stays — OS privilege brokers are the correct mechanism, not a dependency to remove. Document this as a non-goal. | L | any |

## 4. Explicit non-goals of this phase

- GUI (unchanged from v1: the crate split already leaves room).
- Windows-as-host (Rufus already serves that audience).
- Rewriting pure-Rust crate dependencies (see §2).
- `install.wim` re-compression or image editing — Argos copies media, it does
  not build it.

## 5. GitHub synchronization

Each milestone is tracked as one GitHub issue carrying the milestone's task
table as a checklist, labeled with `phase3` plus difficulty
(`diff:critical`/`diff:high`/`diff:medium`/`diff:low` — the *hardest* task in
the milestone) and host (`host:linux`/`host:macos`/`host:any`/`host:real-hw`)
labels, so work can be routed to the right model/human on the right machine.
Existing issues are reused where they already cover a milestone: #40 is M1,
#34 is repurposed as M4 (its macFUSE approach superseded by this plan), #35
is M7.5. A phase-3 tracking issue links them all, the same role #27 played
for phase 2.
