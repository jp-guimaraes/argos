# `uefi-ntfs.img` provenance

Vendored for the UEFI:NTFS write path (backlog #27, W3 -- see
`docs/architecture.md`'s phase 2 guiding decisions). Rewritten from scratch by
`cargo xtask vendor-uefi-ntfs`, never fetched pre-built: unlike what Argos's
initial phase 2 planning assumed, [`pbatard/uefi-ntfs`][upstream] does not
publish a ready-made disk image, only the signed `.efi` driver stubs
themselves as release assets. Rufus assembles its own `res/uefi/uefi-ntfs.img`
from the same assets at Rufus-release time; this is Argos's equivalent.

[upstream]: https://github.com/pbatard/uefi-ntfs

## Source files (`uefi-ntfs-src/`)

Downloaded verbatim, unmodified, from
[`pbatard/uefi-ntfs` release v2.8](https://github.com/pbatard/uefi-ntfs/releases/tag/v2.8):

| File | SHA-256 |
|---|---|
| `bootx64_signed.efi` | `5e22e6209ea557fce49cdbab7d06be4fc99e65d45c4fba01da928e763776bb94` |
| `bootia32_signed.efi` | `32f7c8cb505ce7b32f560a9c51fe6abe14361823a46cb1541039cb52164769c1` |
| `bootaa64_signed.efi` | `2a991a37ddfccd8152b043c3cc507bf578708ffb9f8f4c84c72a919d6c4457e3` |

The `_signed` variants are used (rather than the plain `boot*.efi` ones also
published in that release) because they carry Microsoft's UEFI CA signature,
needed to boot on machines with Secure Boot enabled -- the same variants Rufus
vendors.

`pbatard/uefi-ntfs` compiles [`ntfs-3g`](https://github.com/tuxera/ntfs-3g)
into a UEFI driver; `ntfs-3g` itself is GPL-2.0-or-later. `uefi-ntfs` and these
compiled binaries carry the same license. Argos vendors and redistributes them
unmodified, with this attribution, the same posture Rufus itself takes.

## `uefi-ntfs.img`

Built by:

```sh
cargo xtask vendor-uefi-ntfs crates/argos-privileged/assets/uefi-ntfs-src \
  crates/argos-privileged/assets/uefi-ntfs.img
```

A 1.44 MB FAT12 image (the classic floppy-disk size Rufus's own image has long
used for this) with each stub copied to its architecture's standard UEFI
fallback boot path:

```
EFI/BOOT/BOOTX64.EFI   (x64)
EFI/BOOT/BOOTIA32.EFI  (IA32)
EFI/BOOT/BOOTAA64.EFI  (ARM64)
```

`argos-helper` writes this image verbatim (`dd_mode::write_stream`) as
partition 1 of a Windows installer write -- see `argos_privileged::windows`.
It is never parsed, formatted, or modified at Argos's own runtime.

SHA-256 of the current `uefi-ntfs.img`:

```
baa039115aa894748c1600810c21dddd5826dd62c160aca9a152d428737cf73f
```

## Updating

When upstream cuts a new `uefi-ntfs` release worth picking up: download the
three `*_signed.efi` assets from its GitHub release page, replace the files in
`uefi-ntfs-src/`, rebuild with the command above, and update the checksums and
release tag/link in this document.
