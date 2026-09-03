# Plan: closing phase 3 on Linux hosts (M5.1 and what follows)

Written 2026-09-02, after phase 3's macOS side reached real-hardware
validation. This is the working plan for the agent operating on Linux; it
assumes [`architecture.md`](architecture.md) and
[`plan-phase3-self-contained.md`](plan-phase3-self-contained.md) and does not
restate them.

## 1. Where things actually stand

Everything below was confirmed by booting real machines, not by argument.

| Path | Host | Firmware | State |
|---|---|---|---|
| `--layout fat32` (GPT) | macOS | UEFI | **Setup reaches disk selection**, with an unsplit `install.esd` and with a split `install.wim` |
| `--layout fat32-bios` (MBR) | macOS | legacy BIOS | **Setup reaches disk selection** (Intel Atom N455, AMI BIOS 2011) |
| `--layout fat32` / `fat32-bios` | **Linux** | either | **Unvalidated on real hardware.** This is M5.1, and it is the gap this plan exists to close |
| `--layout ntfs` | Linux | UEFI | Works; superseded, pending the M4.3 retire-or-keep decision |

Nothing in the FAT32 path is platform-specific: there is no `mkfs`, no mount,
no partition device node and no kernel filesystem driver involved. The write
goes straight into the partition's byte range of the open device fd. So the
Linux result is *expected* to match macOS — which is exactly why it needs
testing rather than assuming.

## 2. Tasks, in order

### L1 — Re-open the FAT32 conformance PR (#57) — **done, PR #61**

GitHub closed it when the stack merge deleted its base branch, and a closed PR
can be neither reopened nor retargeted. The branch `fat32-linux-conformance`
survives on the remote with commit `4f4f965` intact. Open a fresh PR from it
against `main`.

Reopened as **#61**. The commit was cherry-picked onto current `main` rather
than rebased (see the trap below — cherry-picking the single commit sidesteps
it entirely) and revalidated there, `main` having since gained the MBR/BIOS
layout and the BPB fixes.

**Trap when rebasing:** the stack was merged with squash commits, so every SHA
`main` absorbed is new. Git will report `add/add` conflicts on files whose
content is in fact identical. Before resolving any of them, check:

```sh
git diff origin/main:<file> <inherited-commit>:<file>
```

Empty output means `main` holds exactly what the branch already inherited, and
the branch's own version is the correct resolution.

### L2 — M5.1: real-hardware validation from a Linux host

The substance of this plan. Write from a Linux machine and boot the result:

1. `--layout fat32` to a UEFI machine
2. `--layout fat32-bios` to a legacy BIOS machine

Acceptance is the same criterion the macOS side met: **Windows Setup reaching
disk selection**, which is what proves the split `.swm` is accepted. Stopping
at the language screen proves only that the firmware found a bootloader.

Use a Windows 10 ISO whose `install.wim` exceeds 4 GiB, so the splitter is
actually exercised. `.testdata/Win10_22H2_English_x64v1.iso` is one.

### L3 — Confirm the stale-GPT fix under Linux's own device handling

The bug that cost this phase several lab rounds: `mbrman` writes sector 0 and
nothing else, so a stick previously written with `--layout fat32` kept its
whole GPT under a non-protective MBR, and Windows then refused the volume a
drive letter. Fixed in #59.

Reproduce the *scenario* on Linux — write `--layout fat32` to a stick, then
`--layout fat32-bios` to the same stick — and confirm with

```sh
sudo ./tools/mediadiff.py dump /dev/sdX --label RECYCLED
```

that no GPT header remains at LBA 1 and none in the device's last sector. The
tool reports the detected scheme; a device just written as MBR reporting `GPT`
is the failure signature.

Worth checking specifically on Linux because the kernel caches partition
tables and re-reads them on `BLKRRPART`, so a stale GPT can surface
differently there than on macOS.

**Automated half done, PR #62**: `recycled_device_gpt.rs` recycles one loop
device from `fat32` to `fat32-bios`, detaching and re-attaching between writes
so each probe reads the medium rather than a cached table, and asks
`libblkid` (`blkid -p`) what the device carries — an opinion with no stake in
our writer, and the same one `lsblk` and udev act on. It attaches *with*
`--partscan`, unlike `write_windows_fat32.rs`, because the kernel parsing the
table is the point. Raw signatures at LBA 1 and the last sector are asserted
too. Still open: the same scenario on a physical stick, confirmed with
`mediadiff.py`.

### L4 — `fatfs`'s directory-entry defects (#56) — **decided**

Argos repairs them after the fact in `repair_directory_entries`.

**Decision: the repair pass stays, and we wait for a release.** There is
nothing to contribute upstream and nothing to vendor: `rust-fatfs` master (an
unreleased 0.4.0, actively maintained) already fixes both defects, verified by
compiling the same reproduction against each version — `..` is cluster 2 under
0.3.6 and 0 under master, and `fsck.vfat -n` exits 1 on the first and 0 on the
second. Master also initializes the FSINFO free-cluster count, the one note
our media still draws. Adopting master early is not a version bump: it
replaced the I/O surface with its own `IoBase`/`ReadWriteSeek` traits, so
`windows_fat32` would need reworking. Rationale and evidence recorded in
`architecture.md` (end of the `argos-privileged` section); when 0.4.0 ships,
`repair_directory_entries` and the FSINFO note should go with it.

### L5 — M4.3: retire the NTFS layout, or keep it

Gated on M5 being complete on both hosts. Retiring it removes the last
`mkfs.ntfs`/`ntfs-3g` shell-outs and the vendored `uefi-ntfs.img` blob, which
is the entire point of phase 3. The argument for keeping it is that NTFS has
no 4 GiB file limit and needs no splitting at all.

### L6 — Independent of everything above

- **M7 (#46)**: shell-out reduction and robustness
- **#35**: end-to-end write cancellation, still unwired

## 3. Conventions and traps

- **Branch plus PR, never straight to `main`.** Two agents work this
  repository.
- **Squash merge** is the convention; `main` stays linear.
- Recent history shows the cost of the alternative to real-hardware testing:
  **six separately-confirmed real defects were fixed while chasing one symptom,
  and none of them was its cause.** Finding a defect is not finding the cause.
  Test before concluding.
- **A recycled USB stick is not a clean target.** Several days were lost to a
  bug that only reproduces on a device that carried a different layout before.
  The QEMU harness never caught it because it builds media in a freshly
  truncated file.
- `tools/mediadiff.py` dumps a device's full structure and diffs two dumps.
  When a hypothesis is about bytes on the medium, it answers in a second and
  the lab does not have to.
